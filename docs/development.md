# 开发指南

## 一、仓库结构

```
apps/moxie/
├── src/
│   ├── main.rs        # 入口：run_with_setup + Router 注册（唯一接触框架的地方）
│   ├── models.rs      # DynamoDB 实体 + API 请求/响应
│   ├── handlers.rs    # 路由层：认证、多租户校验、下发编排
│   ├── auth.rs        # argon2 密码、设备令牌、输入校验
│   ├── tts.rs         # Polly 合成 + 拼音 SSML + 内容寻址
│   └── ocr.rs         # 拍照识别：Bedrock（可选）→ GLM-4V
├── frontend/
│   ├── parent.html    # 家长端
│   └── child.html     # 孩子端
├── infra/
│   ├── template.yaml  # CloudFormation
│   ├── deploy.sh      # 部署
│   ├── issue-cert.sh  # 首次签发 + 导入 ACM
│   └── renew-acm.sh   # 续期后同步 ACM / CloudFront
└── verify.py          # 端到端验证

crates/
├── core/              # 运行时、中间件、JWT、配置、SSM
├── dynamo/            # DynamoDB 薄封装
└── s3/                # S3 封装
```

`crates/` 是从 Operon 无服务器框架抽出的运行时核心，随本仓库一起构建，不需要外部依赖。

## 二、技术栈

| 层 | 选择 |
|---|---|
| 语言 | Rust（edition 2021，MSRV 1.75） |
| Web | axum 0.8 |
| 运行时 | Lambda Web Adapter（本地与云端同一份二进制） |
| 数据库 | DynamoDB 单表 |
| 对象存储 | S3 |
| CDN | CloudFront + OAC |
| TTS | AWS Polly（neural，拼音 SSML） |
| OCR | GLM-4V（默认）/ Bedrock（可选） |
| 前端 | 原生 HTML/CSS/JS，无构建 |

## 三、本地开发

见 [部署与运维 §本地开发](deployment.md#四本地开发)。要点：

- 应用直接使用真实 AWS 服务，没有内置 mock；
- 本地模式（不设 `OPERON_SECRETS_PATH`）用 `OPERON_DEV_JWT_SEED` 签 JWT；
- 需要一张 DynamoDB 表（`OPERON_TABLE_PREFIX` + `data`）和一个 S3 音频桶（`OPERON_MOXIE_AUDIO_BUCKET`）；
- `PORT` 默认 8080。

常用命令：

```bash
cargo test --workspace
cargo test -p operon-moxie
cargo run -p operon-moxie
RUST_LOG=debug cargo run -p operon-moxie
```

## 四、代码约定

### 后端

- **新增路由三步**：
  1. `main.rs` 的 `Router::new()` 注册；
  2. `handlers.rs` 加 handler；
  3. `models.rs` 加请求 / 响应结构。
- **handler 约定**：`Extension(state): Extension<AppState>` 取状态，返回 `Result<Json<T>, AppError>`；
- **AppState 用 `Extension` 注入**：axum 0.8 的 `serve` 只接受 `Router<()>`；
- **数据模型不直接序列化给客户端**：用 `*Out` / `*Resp`，避免泄露 `password_hash`、`device_token_hash`；
- **多租户**：任何带 `{kid_id}` 的 handler 必须先调 `owned_kid()`；
- **错误**：统一 `AppError` 六个变体，返回 `{"error":{"code","message"}}`。

### 前端

- API 一律相对路径 `/api/...`；
- 带 body 的请求必须计算并携带 `x-amz-content-sha256`；
- 错误统一读 `data.error?.message`；
- 所有 `innerHTML` 插入过 `escapeHtml()`；
- 孩子端：词面不进 DOM；大按钮；Wake Lock；`touch-action: manipulation`。

### 注释

代码注释主要解释**为什么**，尤其是：

- 为什么孩子用独立分区；
- 为什么设备令牌不用 JWT；
- 为什么音频 key 要长度前缀；
- 为什么 OCR 必须过滤「含汉字」而不是「非 ASCII」；
- 为什么两遍版用 SSML `<break>`。

修改这些逻辑前先读注释，避免重新踩坑。

## 五、测试

**铁律：新增 / 修改逻辑必须同步加单元测试，`cargo test` 全绿才提交。**

测试不依赖 AWS：

- 纯逻辑 → 模块内 `#[cfg(test)] mod tests`；
- 密码 / 令牌 / 内容寻址 / SSML / OCR 解析 → 纯函数测试；
- handler 认证 / 越权 → 可继续扩展 `tower::ServiceExt::oneshot` 做 HTTP 层测试（当前认证检查位于 DB 调用之前，可直接测）。

现有测试重点：

| 模块 | 覆盖 |
|---|---|
| `models.rs` | 孩子端会话不泄露汉字 / 标题；KidOut 不含令牌哈希；绑定码字符集 |
| `auth.rs` | argon2 加盐 / 非明文 / 损坏哈希不 panic；手机号与密码校验；令牌往返 / 租户隔离 / 哈希 |
| `tts.rs` | key 确定性 / 拼音变化 / 遍数 / 拼接碰撞；SSML 转义与 `<break>`；重试与退避 |
| `ocr.rs` | 编号 / 多词 / 拼音（含带调）过滤 / 去重 / 超长；提示词关键约束；图片解码 |
| `handlers.rs` | 常量时间比较；词表规范化去重；key 反查；改拼音换 key |

部署后跑端到端：

```bash
python3 apps/moxie/verify.py https://<your-cloudfront-domain>
```

## 六、如何扩展

### 换 TTS 提供商

改 `tts.rs` 的三个函数即可，其他模块不需要动：

1. `audio_key()`：保持内容寻址语义（更换音色时把音色标识纳入哈希）；
2. `build_ssml()`：构造对应提供商的读音标注；
3. `synthesize()`：返回 MP3 字节。

注意保持「每词两版（1 遍 / 2 遍）」和幂等 key。

### 换 OCR 提供商

改 `ocr.rs` 的 `recognize()`：

- 新增 provider 函数，返回模型原始文本；
- 继续复用 `parse_words()`（它只保留含汉字的 token）；
- 保持「提示词明确要求提取一个格子里的所有词」这条准确率关键约束，并更新对应测试。

### 加新路由

见上文「新增路由三步」。记得：

- 若是家长接口，加 `JwtAuth` 并做 `owned_kid()`；
- 若是孩子接口，用 `device_context()` 校验 `X-Device-Token`；
- 补测试。

### 加错词本 / 练习记录

数据模型已预留：

```
pk = KID#{kid_id}  sk = PRACTICE#{零填充时间戳}
pk = KID#{kid_id}  sk = WRONG#{word_hash}
```

注意：同一 `pk` 下的记录会用 `query()` 反序列化成同一类型。若类型不同，像孩子分区一样再拆一个前缀分区。

## 七、版本与提交

- 提交信息建议用 Conventional Commits：`feat: ...` / `fix: ...` / `docs: ...` / `test: ...`；
- 一次提交尽量只做一件事；
- 提交前：

```bash
cargo test --workspace
# 可选：cargo fmt --all
```

- 发布前检查是否误提交 `.env`、密钥、个人域名或账号 ID。

更多见 [CONTRIBUTING.md](../CONTRIBUTING.md)。