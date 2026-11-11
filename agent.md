# Agent 工作约定

本文件面向在本仓库中工作的 AI 编码代理。以下约定对本仓库的所有提交生效。

## 一、提交信息（硬性规则）

1. **提交信息中不得包含任何 AI 署名、工具归因或自动生成标记**，包括但不限于：
   - `Co-Authored-By: Claude <noreply@anthropic.com>`
   - `Generated with Claude Code`
   - `🤖 Generated with ...`
   - 任何形式的 `Co-Authored-By` / `Signed-off-by` AI 署名
2. 提交信息只描述**人**要解决的变更本身；不要提及使用了哪个模型或工具。
3. 使用 Conventional Commits 前缀：

   ```
   feat: 支持一次下发多个孩子
   fix: 修复解绑后旧设备令牌仍可读答案
   docs: 补充自定义域名证书续期说明
   test: 覆盖带声调拼音的 OCR 过滤
   chore: 升级依赖
   ```

4. 一次提交尽量只做一件事；提交信息说明「为什么」，正文可列要点。

## 二、提交前检查

- `cargo test --workspace` 必须全绿（新增 / 修改逻辑必须同步加测试）。
- 不提交任何密钥、`.env`、个人域名、AWS 账号 ID、CloudFront Distribution ID、证书 ARN 或个人路径。
- 前端保持零构建、零外部依赖。
- 涉及认证、多租户、防偷看、密钥的改动，必须在说明中写清威胁模型变化。

## 三、代码约定速查

- `main.rs` 只负责路由注册；业务编排放 `handlers.rs`；纯逻辑放 `auth.rs` / `tts.rs` / `ocr.rs`。
- 任何带 `{kid_id}` 的家长接口，必须调用 `owned_kid()` 校验归属。
- 孩子端响应不得包含 `text` / `pinyin` / `title` 或任何汉字。
- 面向客户端的结构不得包含 `password_hash`、`device_token_hash`。
- 注释解释「为什么」，尤其是内容寻址、设备令牌撤销、独立分区的设计原因。

## 四、当前仓库信息

- 默认分支：`main`
- 远端：`git@github.com:shinvdu/moxie.git`
- 项目：小学生语音默写小工具（Rust + Lambda + DynamoDB + S3 + CloudFront）

更完整的开发说明见 [docs/development.md](docs/development.md) 与 [CONTRIBUTING.md](CONTRIBUTING.md)。