# moxie · 小学生语音默写小助手

[![CI](https://github.com/shinvdu/moxie/actions/workflows/ci.yml/badge.svg)](https://github.com/shinvdu/moxie/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> **每报一个词，等孩子写完，再报下一个。**

moxie 是一个面向小学语文「默写 / 听写」场景的开源小工具。家长在手机上把当天要默写的词语远程下发到孩子的平板，孩子点一下屏幕就念下一个词——**节奏完全由孩子掌握**，不需要家长守在旁边念。

它最初是为一个四年级小朋友的每日作业做的，已经实际部署使用。本项目把完整的需求、设计、代码、基础设施和踩坑记录都开源出来，方便其他家长或开发者自建。

---

## 它解决什么问题

网上大多数听写 App 是**固定间隔自动播**：间隔设 20 秒，写得快的小孩干等，写得慢的跟不上。

moxie 的选择是：

- **纯手动推进**：孩子写完才点「下一个」，整屏都是热区；
- **一次只报一个词**：可重读、可回上一个、可切 1 遍 / 2 遍；
- **屏幕上没有任何汉字**：孩子端 API 从服务端开始就不下发词面，打开开发者工具也看不到，没法照着抄；
- **家长远程下发**：家长在自己手机上录入词表（粘贴文本或拍照 OCR），孩子平板刷新即可开始；
- **多音字可纠错**：用 AWS Polly 的拼音 SSML 强制指定读音（如「模糊 mó」而不是「mú」），读错可逐词返工。

---

## 架构

```
                         CloudFront
                              │
        ┌─────────────────────┼─────────────────────┐
        │                     │                     │
    /  (前端)             /api/* (API)          /audio/* (音频)
        │                     │                     │
   S3 前端桶              Lambda 单函数          S3 音频桶
   parent.html            （Rust + axum +         （内容寻址，不可变，
   child.html              Lambda Web Adapter）    跨用户共享）
                              │
                 ┌────────────┼────────────┐
                 │            │            │
             DynamoDB      Polly        GLM-4V / Bedrock
          账号 / 孩子 /   语音合成       拍照 OCR
            词表 / 绑定码
```

- **后端**：Rust + axum + AWS Lambda Web Adapter，单 Lambda 打天下；
- **数据库**：DynamoDB 单表（`pk`/`sk`），按需计费，无 GSI；
- **音频**：Polly 预生成 MP3 写 S3，key 由 `(文本, 拼音, 遍数, 音色)` 哈希得出——**全平台同一份，第 1000 个用户下发同一课词表时 Polly 调用为 0**；
- **前端**：原生 HTML/CSS/JS，无框架、无构建，两个自包含页面；
- **基础设施**：一份 CloudFormation 模板，`./deploy.sh dev` 一条命令部署。

### 核心设计点

| 问题 | 做法 |
|---|---|
| 等孩子写完再报下一个 | 纯手动按钮，不自动播 |
| 多音字读错 | Polly 拼音 SSML + 家长逐词试听、按需补拼音 |
| 孩子偷看答案 | 孩子端会话响应里**不含任何汉字**（连标题都不下发） |
| 换平板 / 平板丢了 | 设备令牌是不透明随机串，库里只存 sha256，家长解绑即刻失效 |
| 绑定码被猜 / 重放 | 6 位无歧义字符 + 5 分钟有效 + 用完即删 |
| 多租户越权 | 每个 `{kid_id}` 操作都按当前账号的分区取记录，别人的一律 404 |
| 音频成本随用户增长 | 内容寻址 + 跨用户共享，Polly 成本趋近零 |
| 拍照图片过大 | 浏览器 canvas 压缩到长边 1600px 再上传（CloudFront 请求体上限约 1.5–2MB） |

---

## 快速开始

### 1. 本地编译与测试

```bash
git clone git@github.com:shinvdu/moxie.git
cd moxie

cargo test --workspace      # 单元测试，无需 AWS 凭证
cargo build --release -p operon-moxie
```

### 2. 部署到 AWS

前置：AWS 账号、`cargo` / `rustup`、`zip`、`python3`、AWS CLI v2。

```bash
cp .env.example .env        # 填入 AWS 凭证与区域
cd apps/moxie/infra
./deploy.sh dev             # 编译 musl → 打包 → 上传 → CloudFormation → 上传前端
```

部署完成后脚本会打印：

```
站点:    https://xxxxxxxxxxxx.cloudfront.net
孩子端:  https://xxxxxxxxxxxx.cloudfront.net/child.html
家长端:  https://xxxxxxxxxxxx.cloudfront.net/parent.html
```

首次部署会在 SSM 自动生成 `/<project>/<env>/jwt_seed`。若要启用拍照识别，在 `.env` 里设置 `ZHIPU_API_KEY`（也可之后手动写入 SSM）。

> 默认区域 `us-west-2`，可在 `.env` 的 `AWS_DEFAULT_REGION` 修改。自定义域名与 HTTPS 证书见 [部署文档](docs/deployment.md#自定义域名与-https)。

### 3. 用起来

1. 家长打开 `/parent.html`，注册账号、添加孩子；
2. 点「绑定平板」，得到一个 5 分钟有效的绑定码；
3. 孩子平板打开 `/child.html`，输入绑定码，然后「添加到主屏幕」；
4. 家长录入词表 → 逐词试听 → 下发；
5. 孩子点图标 → 开始默写 → 写完点「下一个」。

详细操作见 [使用说明](docs/usage.md)。

---

## 文档

| 文档 | 内容 |
|---|---|
| [需求与调研](docs/requirements.md) | 场景、目标、非目标、现成方案调研、首课词表 |
| [技术设计](docs/design.md) | 架构、技术选型、数据模型、核心流程、实施中的真实取舍 |
| [API 参考](docs/api.md) | 全部接口、认证头、错误格式、示例 |
| [部署与运维](docs/deployment.md) | AWS 资源、部署、自定义域名、日志、密钥轮换、删除环境 |
| [使用说明](docs/usage.md) | 家长端 / 孩子端逐步操作、界面说明、小技巧 |
| [费用测算](docs/cost.md) | 10 / 100 / 1,000 / 10,000 家庭的实测锚点与敏感性分析 |
| [安全设计](docs/security.md) | 多租户隔离、防偷看、密码哈希、设备令牌撤销、已知限制 |
| [常见问题](docs/faq.md) | 使用问题、部署排坑、发音校对结论 |
| [开发指南](docs/development.md) | 仓库结构、本地开发、测试约定、如何加路由 / 换 OCR 提供商 |
| [贡献指南](CONTRIBUTING.md) | 提交规范、测试要求 |

---

## 仓库结构

```
moxie/
├── apps/moxie/
│   ├── src/
│   │   ├── main.rs        # 入口 + 路由注册
│   │   ├── models.rs      # DynamoDB 实体 + API 请求/响应
│   │   ├── handlers.rs    # 路由层：认证、多租户校验、下发编排
│   │   ├── auth.rs        # argon2 密码、设备令牌、输入校验
│   │   ├── tts.rs         # Polly 合成 + 拼音 SSML + 内容寻址
│   │   └── ocr.rs         # 拍照识别：Bedrock（可选）→ GLM-4V
│   ├── frontend/
│   │   ├── parent.html    # 家长端：注册/登录/孩子/词表/试听/下发
│   │   └── child.html     # 孩子端：大按钮、防偷看、屏幕常亮
│   ├── infra/
│   │   ├── template.yaml  # CloudFormation：DDB + S3 ×2 + Lambda + CloudFront
│   │   ├── deploy.sh      # 端到端部署
│   │   ├── issue-cert.sh  # 自定义域名证书签发 + 导入 ACM
│   │   └── renew-acm.sh   # 证书续期 → ACM → CloudFront
│   └── verify.py          # 部署后端到端验证（59 项断言）
├── crates/
│   ├── core/              # 运行时、中间件、JWT、配置、SSM
│   ├── dynamo/            # DynamoDB 薄封装
│   └── s3/                # S3 封装（put/get/presign）
├── samples/
│   ├── words/fanxing.json # 第 2 课《繁星》词表示例
│   ├── ocr/               # 生字表照片 + 期望识别结果
│   └── pronunciation/     # Polly 发音校对样本（默认读音 / 多音字 A-B 对照）
├── docs/
└── .github/workflows/ci.yml
```

`crates/` 是从 [Operon 无服务器框架](https://github.com/shinvdu/operon) 抽出的运行时核心，随本项目一起以 MIT 许可发布，仓库可独立构建。

---

## 测试

```bash
cargo test --workspace        # 全部单元测试
cargo test -p operon-moxie    # 只测应用
```

当前共 61 个单元测试（moxie 59 + 框架 crate 2），不依赖 AWS：密码 / 令牌 / 内容寻址 / SSML / OCR 解析 / 防偷看序列化等都是纯逻辑，越权与认证检查位于数据库调用之前。部署后可跑端到端脚本：

```bash
python3 apps/moxie/verify.py https://<your-cloudfront-domain>
```

覆盖注册登录、下发、缓存命中、绑定码、孩子端无汉字、越权、解绑失效等 59 项断言。

---

## 成本

音频内容寻址让成本几乎不随用户数线性增长。以每家庭每月 12 课、每课 20 词、30% 拍照录入估算：

| 规模 | 首年月成本 | 次年月成本 | 每家庭月成本 |
|---|---|---|---|
| 10 家庭 | ≈ $0.07 | ≈ $0.08 | < $0.01 |
| 100 家庭 | ≈ $0.57 | ≈ $0.70 | ~$0.006 |
| 1,000 家庭 | ≈ $5.4 | ≈ $6.7 | ~$0.007 |
| 10,000 家庭 | ≈ $53 | ≈ $67 | ~$0.007 |

最大的单项不是语音合成，而是**拍照 OCR**——它是唯一没有共享机制的环节。完整假设、实测锚点和优化建议见 [费用测算](docs/cost.md)。

---

## 已知限制

- 无注册 / 登录限流，产品化前需要补；
- 手机号未做短信验证；
- 音频 URL 靠「文件名不可猜」防护，未上签名 URL；
- 错词本、练习记录 schema 已预留，尚未实现；
- Lambda 超时 30 秒，首次下发超大词表（>100 词）可能偏紧，已用 8 路并发缓解。

---

## License

[MIT](LICENSE)