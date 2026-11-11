# 技术设计

> 版本：v2（加入账号体系与产品化设计）　｜　状态：已实现并端到端验证
>
> 配套阅读：[需求与调研](requirements.md)、[API 参考](api.md)、[安全设计](security.md)

## 一、设计目标与约束

### 目标

- 家长手机远程下发、孩子平板顺序播报；
- 一次只报一个词，孩子手动推进；
- 防偷看：孩子端拿不到词面；
- 多音字可纠错；
- 多租户账号体系，孩子设备可绑定 / 解绑；
- 无服务器，家庭级成本趋近零，产品化后不随用户数线性增长。

### 关键约束

| 约束 | 影响 |
|---|---|
| Lambda Function URL 有 6MB payload 上限，且没有二进制响应接口 | **音频绝不能走 Lambda**，必须 CloudFront → S3 直连 |
| Lambda 超时 30 秒 | 下发时生成音频必须并发化 |
| CloudFront 请求体上限约 1.5–2MB | 手机原图必须浏览器端压缩后再上传 |
| CloudFront OAC 占用标准 `Authorization` 头 | 业务 JWT 走 `X-Authorization` |
| 教材词表在用户间高度重复 | 用内容寻址做跨用户音频共享，把 TTS 成本变成一次性 |

---

## 二、系统架构

```
                        CloudFront
                              │
        ┌─────────────────────┼─────────────────────┐
        │                     │                     │
    /  (前端)             /api/* (API)          /audio/* (音频)
        │                     │                     │
   S3 前端桶              Lambda moxie           S3 音频桶
   parent.html                 │                 (内容寻址、不可变、跨用户共享)
   child.html                  │
                    ┌──────────┼──────────┐
                    │          │          │
                DynamoDB    Polly      GLM-4V / Bedrock
              账号/孩子/词表  语音合成    拍照 OCR
```

- **单 Lambda 打天下**：Rust + axum + Lambda Web Adapter，本地与云端同一份二进制；
- **三个 CloudFront behavior**：`/api/*` → Lambda，`/audio/*` → 音频桶，默认 → 前端桶；
- **音频不经 Lambda**：既避开 6MB 限制，也让分发边际成本几乎为零。

---

## 三、技术选型

### 3.1 TTS：AWS Polly

| 候选 | 结论 |
|---|---|
| **AWS Polly** ✅ | 官方服务、IAM 认证、无外部依赖、支持拼音 SSML、成本可忽略 |
| edge-tts | 音色更多，但非官方端点、无 SLA，且需从 Lambda 外连微软服务 |
| 浏览器 `SpeechSynthesis` | 音色由平板决定，国产平板常缺中文引擎，**多音字完全不可控** |

`cmn-CN` 目前只有一个音色 Zhiyu（女声），支持 neural。默写重视清晰度而非音色多样性，够用。

### 3.2 多音字：Polly 拼音 SSML

```xml
<speak>
  <phoneme alphabet="x-amazon-pinyin" ph="mo2 hu2">模糊</phoneme>
</speak>
```

声调数字：1 阴平 / 2 阳平 / 3 上声 / 4 去声 / 0 轻声。

实测对照（词：模糊）：

| 生成方式 | 大小 | SHA256 前 8 位 |
|---|---|---|
| 无标注（Polly 自判） | 4364 B | `8f2bcdfa` |
| `<phoneme ph="mo2 hu2">`（正确） | 4940 B | `70a23dab` |
| `<phoneme ph="mu2 hu2">`（错误） | 4796 B | `e731b8c7` |

三者哈希完全不同，证明 `x-amazon-pinyin` 确实在改变发音。

**纠错闭环**：拼音不必一开始就录全。默认交给 Polly 自判，家长试听发现读错再补拼音重新生成。首课 5 个风险词的实测结果是 Polly 默认读音全部正确。

### 3.3 音频存储：内容寻址

音频 key：

```
audio/{sha256(text | pinyin | repeat | voice)[:16]}.mp3
```

字段用长度前缀喂入哈希，避免 `("ab","c")` 与 `("a","bc")` 这类拼接歧义。

**为什么内容寻址重要：**

- 同一词在全平台只生成一次，跨用户、跨课文、跨天复用；
- 全国小学生学同一套教材，命中率极高：第 1000 个用户下发同一课时 **Polly 调用为 0**；
- 内容不可变 → CloudFront 可长缓存；
- 幂等 → 重复下发秒完成；
- 改了拼音 → key 必变 → 不会继续命中旧的错误音频。

### 3.4 OCR：Bedrock（可选）→ GLM-4V（默认）

首选 AWS Bedrock（原生 AWS、IAM 认证、免第三方 key），降级 GLM-4V（key 存 SSM）。

**实施发现**：部分 AWS 账号未通过 Bedrock 的 allowlisting，调用 Anthropic 模型会报地区限制，调用 Amazon Nova 会要求企业身份验证。因此：

- Bedrock 代码保留，但改成**显式 opt-in**：只有配置了 `OPERON_MOXIE_BEDROCK_MODEL_ID` 才尝试；
- 默认走 **GLM-4V**，key 存 SSM 的 `zhipu_api_key`；
- 若账号通过了 allowlisting，导出环境变量即可启用（建议用 Amazon Nova）。

**模型选型实测**（同一张真实生字表）：

| 模型 | 提示词 | 准确率 | 耗时 |
|---|---|---|---|
| `glm-4v-flash`（免费） | 初版 | 8/19 | 1.5 s |
| `glm-4v-flash`（免费） | 改进后 | 14/19 | 1.7 s |
| **`glm-4v-plus`** | **改进后** | **19/19** | **2.5 s** |
| `glm-4.5v` | 改进后 | 19/19 | 10.8 s |

两个结论：

- 提示词必须明说「一个格子里可能有多个词语」并举例（「繁 → 繁星 繁华」），否则模型只取每行第一个词，还出现过「谈吐 → 谈话」这类错字；
- `glm-4v-plus` 是最优点：满分且快。

### 3.5 交付形态

| 形态 | 评价 |
|---|---|
| **Web 页面（S3 + CloudFront）** ✅ | 平板浏览器直接打开，「添加到主屏幕」接近 App；无安装、无上架 |
| 自包含单文件 HTML | 完全离线，但无法远程下发 |
| PWA | 体验更好，但需要额外 manifest / Service Worker；当前未做 |
| APK | WebView 套壳，除非上架否则不划算 |

---

## 四、数据模型

DynamoDB 单表 `{project}-{env}-data`（`pk` / `sk`，按需计费）。前缀式 `pk` 设计，按 `pk` 查询即可取回一类记录。

### 4.1 账号

```
pk = USER#{user_id}
sk = PROFILE
{
  "user_id": "u_8f3a...",
  "phone": "13800138000",          // 登录名，唯一
  "password_hash": "<argon2id>",   // 绝不明文
  "nickname": "家长昵称",
  "created_at": 1786701600
}
```

手机号唯一性用独立索引项：

```
pk = PHONE#{phone}
sk = META
{ "user_id": "u_8f3a...", "created_at": 1786701600 }
```

### 4.2 孩子（一个账号多个）

```
pk = USER#{user_id}#KIDS
sk = KID#{kid_id}
{
  "kid_id": "k_2b7c...",
  "user_id": "u_8f3a...",
  "name": "小明",
  "grade": 4,
  "device_token_hash": "<sha256>",  // 未绑定为空
  "bound_at": 1786701600,
  "created_at": 1786701600
}
```

> **为什么孩子用独立分区 `USER#{id}#KIDS`**：Dynamo 封装 `query(pk)` 会把同一分区下所有记录反序列化成同一类型。账号与孩子共用分区会导致 `PROFILE` 与 `KID#` 互相反序列化失败。

### 4.3 当前下发的词表

```
pk = KID#{kid_id}
sk = CURRENT
{
  "title": "第2课《繁星》",
  "pushed_at": 1786701600,
  "pushed_by": "u_8f3a...",
  "default_repeat": 1,
  "words": [
    { "text": "繁星", "pinyin": null,      "a1": "/audio/ab12cd34.mp3", "a2": "/audio/ef56gh78.mp3" },
    { "text": "模糊", "pinyin": "mo2 hu2", "a1": "/audio/...",          "a2": "/audio/..." }
  ]
}
```

20 个词约 3–4 KB，远低于 400 KB 单项上限，词表直接内嵌不拆项。

### 4.4 绑定码

```
pk = BIND#{code}          // 6 位无歧义字符
sk = META
{ "kid_id": "k_2b7c...", "user_id": "u_8f3a...", "expires_at": 1786701900 }
```

校验通过后立即删除，防止重放；过期码在删除后返回错误。

### 4.5 产品化预留（未实现，schema 已支持）

```
pk = KID#{kid_id}  sk = PRACTICE#{零填充时间戳}   // 每次默写：用时、完成度
pk = KID#{kid_id}  sk = WRONG#{word_hash}        // 错词本：错误次数、最近错误时间
```

---

## 五、核心流程

### 5.1 注册与绑定设备（一次性）

```
家长手机                                 孩子平板
   │                                        │
   ├─ 注册/登录 (手机号+密码) → JWT         │
   ├─ 添加孩子「小明」                       │
   ├─ 点「绑定平板」→ 得到 6 位绑定码        │
   │   (5 分钟内有效)                        │
   ├──────── 把绑定码念给孩子 ─────────────▶ │
   │                                        ├─ 打开 /child.html
   │                                        ├─ 输入绑定码
   │                                        ├─ POST /api/kid/bind
   │                                        │   ← {device_token}
   │                                        ├─ 存 localStorage
   │                                        └─ 添加到主屏幕
```

### 5.2 下发（家长每天的操作）

```
家长拍照/粘贴 → 逐词试听 → 点「下发」
   ▼
POST /api/kids/{kid_id}/push
   ▼ Lambda
   1. 校验 JWT，且该 kid 属于当前账号
   2. 逐词计算音频 key（1 遍 / 2 遍两版）
   3. 并发探测 S3，筛出缺失项
      └─ 教材词表跨用户共享，通常全部命中
   4. 并发调 Polly 生成缺失项（限流 8 路）
      └─ 有拼音 → SSML <phoneme>；无拼音 → 纯文本
   5. 上传 S3
   6. 写 KID#{kid_id} / CURRENT
   ▼
{ generated: 0, cached: 38, total: 38 }
```

**为什么推送时生成而不是播放时**：孩子端播放零延迟。命中缓存时家长几乎无等待；全新词表通常 2–5 秒。

**30 秒超时对策**：`buffer_unordered(8)` 限流并发生成；实测 38 段音频约 4 秒（含冷启动）。

### 5.3 孩子端默写

```
打开页面 → 校验设备令牌 → 拉取会话（只有音频路径）→ 预加载全部 <audio>
   ├─ 点「开始默写」→ 播第 1 个词
   ├─ 播完 → 孩子写 → 点「下一个」→ 播 N+1
   ├─ 没听清 → 「重读」→ 重播当前
   ├─ 写错了 → 「上一个」→ 回到前一个
   └─ 切「1遍 / 2遍」→ 换预生成文件
   ▼ 全部完成
   显示「完成！」+「看答案」
```

---

## 六、前端设计

**技术栈**：纯原生 HTML/CSS/JS，单文件自包含，无框架无构建。

### 6.1 必须继承的约定

- API 全用**相对路径** `/api/...`（同源，无需 CORS）；
- **POST 必须带 `x-amz-content-sha256`**（`crypto.subtle.digest('SHA-256', body)`），否则 CloudFront / Function URL 返回 403；
- 错误统一取 `data.error?.message`；
- 所有 `innerHTML` 插入过 `escapeHtml()`。

### 6.2 孩子端专项

| 项 | 做法 |
|---|---|
| 防偷看 | 词面**不进 DOM**，服务端也不下发 |
| 大按钮 | 整屏热区，触摸目标 ≥ 88px |
| 屏幕常亮 | `navigator.wakeLock.request('screen')`（需 HTTPS） |
| 禁误触 | `touch-action: manipulation` 禁双击缩放；禁长按选中 |
| 音频预加载 | 页面加载时批量 `new Audio(url)` + `preload="auto"` |
| 播报两遍 | 切换两个预生成文件，不客户端连播（避免静音停顿） |
| 键盘 | 空格 = 下一个，← = 上一个，R = 重读 |

### 6.3 家长端功能

注册 / 登录 → 孩子列表 → 添加孩子 → 生成绑定码 → 录入词表（粘贴 / 拍照 OCR）→ 解析预览可编辑 → 逐词试听 + 补拼音纠错 → 下发。

拍照上传前用 canvas 压缩到长边 ≤1600px、JPEG 0.75。实测 1672 KB → 74 KB（22 倍压缩）。

---

## 七、基础设施

全部自包含在 `apps/moxie/`，一份 CloudFormation 模板：

| 资源 | 说明 |
|---|---|
| DynamoDB 表 | `{project}-{env}-data`，pk/sk，按需计费，PITR |
| S3 ×2 | 前端桶（版本控制）+ 音频桶（无版本控制，内容不可变） |
| S3 BucketPolicy ×2 | 各自授信对应 CloudFront OAC |
| Lambda | `{project}-{env}-api`，provided.al2023，256MB，超时 30s |
| Lambda Function URL | `AWS_IAM`，两条 invoke 权限（缺任一 403） |
| CloudFront OAC ×3 | Lambda / 前端 S3 / 音频 S3 |
| CloudFront | 三个 behavior：`/api/*`、`/audio/*`、默认 |
| IAM | DynamoDB 读写、SSM 读 + KMS 解密、Polly、Bedrock（可选）、音频桶读写 |

**CloudFront 托管策略 ID：**

| 用途 | CachePolicy | OriginRequestPolicy |
|---|---|---|
| `/api/*` | `4135ea2d-6df8-44a3-9df3-4b5a84be39ad`（CachingDisabled） | `b689b0a8-53d0-40ab-baf2-68738e2966ac`（AllViewerExceptHostHeader，转发自定义头） |
| `/audio/*` | `658327ea-f89d-4fab-a63d-7e88639e58f6`（CachingOptimized） | 不需要 |
| 默认（前端） | `4135ea2d-...`（CachingDisabled） | 不需要 |

---

## 八、实施后记：实测发现与设计偏离

以下 4 处与原设计的偏离全部源于实测，不是偏好变更。

| # | 原设计 | 实际采用 | 原因 |
|---|---|---|---|
| 1 | OCR 首选 Bedrock，降级 GLM-4V | **默认 GLM-4V**，Bedrock 改为显式 opt-in | 部分账号未通过 Bedrock allowlisting，Anthropic 系另有地区限制 |
| 2 | 无重试 | Polly 调用加 **3 次指数退避重试** | 实测 38 段音频中有 2 段偶发 `service error`，不重试会导致整个下发失败 |
| 3 | 孩子端下发 `title` | **连 title 也不下发** | 标题「第2课《繁星》」里的「繁星」正是待默写的词，等于送答案 |
| 4 | 浏览器压缩图片（优化） | 改为**必需步骤** | CloudFront 请求体上限约 1.5–2MB，手机原图 base64 后 2.2MB 会被 413 拒绝 |

其他实测发现：

1. **Polly 会偶发瞬时失败**：首次下发 38 段音频，36 成功、2 失败，报通用 `service error`。加指数退避重试后 38/38 成功。
2. **标题会泄露答案**：孩子端会话响应现在只有音频路径和一个总数，验证脚本有 22 条断言专门检查「不含任何汉字」。
3. **带声调的拼音会被误当成词语**：OCR 解析早期用「丢弃纯 ASCII」过滤模型多吐的拼音，但 `fán` 里的 `á` 不是 ASCII，会被放行。改成「必须含汉字」才保留，并补了回归测试。
4. **`/health` 走 CloudFront 会 403**：CloudFront 只把 `/api/*` 路由到 Lambda，裸路径落到 S3。已加 `/api/health`，`/health` 保留给直连 Function URL。
5. **部署脚本的仓库根路径**：`ROOT` 要按 `infra → moxie → apps → repo-root` 往上三级，算错一级会读不到 `.env`。

---

## 九、风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 越权访问他人孩子数据 | 严重 | 每个 `{kid_id}` 操作强制校验归属；统一 404；测试覆盖（见 [安全设计](security.md)） |
| 密码泄露 | 严重 | argon2id 哈希；绝不明文；日志不打密码 |
| 拼音 SSML 对个别词不生效 | 读音错 | 逐词试听复核；必要时逐音节拆分 |
| 只有单一中文音色 | 音色单一 | 默写重视清晰度，可接受；可用 rate 微调 |
| Bedrock 未开通 | OCR 不可用 | 默认 GLM-4V；OCR 本就是可选功能 |
| 6MB payload 超限 | 拍照上传失败 | 浏览器端 canvas 压缩（必需） |
| Lambda 30s 超时 | 下发失败 | 8 路并发 + 内容寻址复用 |
| 音频 URL 被热链 | 流量费 | 内容寻址不可猜；产品化后可上签名 URL |
| 绑定码被暴力猜解 | 他人设备绑定 | 6 位无歧义字符 + 5 分钟有效 + 用完即删；产品化后加限流 |

---

## 十、验证方式

1. **发音正确性**（无法自动验证）：19 个词逐个试听，重点核对 5 个多音字风险词；
2. **越权防护**：A 账号 JWT 访问 B 账号的 `kid_id`，必须 404；
3. **防偷看**：孩子端 Network 响应中无任何汉字；
4. **设备令牌可撤销**：家长解绑后旧令牌立即失效；
5. **弱网 / 离线**：音频预加载后断网，确认仍能播完；
6. **屏幕常亮**：静置 5 分钟不熄屏；
7. **端到端**：家长下发 → 孩子收到 → 完整默写 → 核对答案；
8. **缓存**：重复下发同一词表，返回 `generated: 0`。

---

## 十一、实施计划（已完成）

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1. 骨架 | workspace + 应用 + 本地 `/health` | ✅ |
| 2. TTS | Polly + SSML + 内容寻址 + 单元测试 | ✅ |
| 3. 发音校对 | 首课 20 词批量生成、逐词试听 | ✅ 全部正确 |
| 4. 账号体系 | 注册/登录/argon2/越权校验 + 单元测试 | ✅ |
| 5. 后端 API | models + handlers + DynamoDB | ✅ |
| 6. 前端双端 | `child.html` + `parent.html` | ✅ |
| 7. 基础设施 | 独立栈 + 域名证书 | ✅ |
| 8. 端到端验证 | 家长下发 → 孩子默写 | ✅ 59/59 |