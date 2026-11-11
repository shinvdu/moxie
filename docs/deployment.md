# 部署与运维

> 目标：从零部署一套完整环境（CloudFront + Lambda + DynamoDB + S3 ×2），并长期运维。

## 一、前置条件

| 项 | 要求 |
|---|---|
| AWS 账号 | 能创建 IAM Role、Lambda、DynamoDB、S3、CloudFront、SSM |
| 工具 | `cargo` + `rustup`、`zip`、`python3`（含 boto3，仅自定义域名需要）、AWS CLI v2 |
| 构建目标 | `x86_64-unknown-linux-musl`（脚本会自动 `rustup target add`） |
| 网络 | 能访问 AWS API；受限网络可在 `.env` 里设置 `PROXY_SCRIPT` |
| 权限 | 首次部署 CloudFormation 需要 `CAPABILITY_NAMED_IAM` |

> 默认区域是 `us-west-2`。换区域时同时修改 `.env` 的 `AWS_DEFAULT_REGION` 和
> `params.json` 的 `WebAdapterLayerArn`（Lambda Web Adapter Layer 是分区域的）。

## 二、一键部署

```bash
cp .env.example .env
# 编辑 .env：填入 AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_DEFAULT_REGION
# 可选：ZHIPU_API_KEY=...（启用拍照识别）

cd apps/moxie/infra
./deploy.sh dev
```

脚本执行 7 步：

1. 确保 SSM 密钥存在：`/<project>/<env>/jwt_seed`（首次自动生成 SecureString）；
2. `cargo build --release --target x86_64-unknown-linux-musl -p operon-moxie`；
3. 打包 `bootstrap.zip`（provided.al2023 要求可执行文件名为 `bootstrap`）；
4. 上传代码到部署桶 `{project}-deploy-{account}-{region}`；
5. `aws cloudformation deploy` 创建 / 更新 `{project}-{env}` 栈；
6. 上传 `parent.html` / `child.html` 到前端桶；
7. 打印站点、CloudFront 默认域名与资源输出。

部署完成后访问：

- 家长端：`https://<domain>/parent.html`
- 孩子端：`https://<domain>/child.html`

## 三、AWS 资源清单

| 资源 | 默认名称 | 说明 |
|---|---|---|
| CloudFormation 栈 | `moxie-dev` | |
| Lambda | `moxie-dev-api` | provided.al2023，256MB，超时 30s |
| Lambda Function URL | — | `AWS_IAM`，只允许 CloudFront OAC 调用 |
| DynamoDB | `moxie-dev-data` | pk/sk 单表，按需计费，PITR |
| S3（前端） | `moxie-dev-frontend` | `parent.html` / `child.html`，开版本控制 |
| S3（音频） | `moxie-dev-audio` | Polly 生成的 MP3，无版本控制 |
| S3（部署包） | `moxie-deploy-<acct>-<region>` | 栈外创建，删除栈时需手动清理 |
| CloudFront | — | 三个 behavior：`/api/*`、`/audio/*`、默认 |
| SSM | `/moxie/dev/jwt_seed` | JWT 签名种子（SecureString） |
| SSM | `/moxie/dev/zhipu_api_key` | OCR 用（可选，SecureString） |

### 部署相关的几个硬性配置

1. **OAC 需要两条 Lambda 权限**：`lambda:InvokeFunctionUrl` 和 `lambda:InvokeFunction`，缺任一都会 403（模板已包含）。
2. **S3 OAC 需要 bucket policy** 授信 CloudFront（模板已包含）。
3. **`/api/*` 必须挂 `AllViewerExceptHostHeader` OriginRequestPolicy**，否则 `X-Authorization` / `X-Device-Token` 不会转发到 Lambda。
4. **POST 必须带 `x-amz-content-sha256`**，否则 403。

## 四、本地开发

moxie 的运行时直接使用真实 AWS 服务（DynamoDB / Polly / S3），没有内置 mock。
本地跑通完整流程的最小做法是：**用同一账号创建一张表和两个桶**，然后指过去。

```bash
# 1) 创建本地开发用的资源（只需一次）
aws dynamodb create-table \
  --table-name moxie-dev-data \
  --attribute-definitions AttributeName=pk,AttributeType=S AttributeName=sk,AttributeType=S \
  --key-schema AttributeName=pk,KeyType=HASH AttributeName=sk,KeyType=RANGE \
  --billing-mode PAY_PER_REQUEST

aws s3 mb s3://moxie-dev-audio --region us-west-2

# 2) 设置环境变量
export OPERON_DEV_JWT_SEED="$(openssl rand -base64 48)"
export OPERON_TABLE_PREFIX="moxie-dev-"
export OPERON_MOXIE_AUDIO_BUCKET="moxie-dev-audio"
export OPERON_DEV_ZHIPU_API_KEY="<可选>"
export AWS_DEFAULT_REGION="us-west-2"
export PORT=8080

# 3) 启动
cargo run -p operon-moxie
```

再另起一个静态服务器托管前端：

```bash
cd apps/moxie/frontend
python3 -m http.server 3000
# 浏览器打开 http://localhost:3000/parent.html
```

前端使用相对路径 `/api/...`，同源部署时才直接可用。本地前后端分端口时，可在浏览器开发者工具里临时改写请求地址，或使用反向代理（如 nginx / Caddy）把 `/api` 与 `/audio` 转发到 8080、静态文件指向 3000。

> 本地模式（未设置 `OPERON_SECRETS_PATH`）下，JWT 种子来自 `OPERON_DEV_JWT_SEED`，
> 不需要 SSM。生产环境由 CloudFormation 注入 `OPERON_SECRETS_PATH`，冷启动时
> 一次 `GetParametersByPath` 拉全部密钥。

## 五、自定义域名与 HTTPS

CloudFront 默认域名可以直接用。要用自己的域名：

### 1. 签发证书

CloudFront **只接受 ACM 证书，且证书必须在 `us-east-1`**（与栈所在区域无关）。

推荐用 Let's Encrypt 的 DNS-01 验证（TXT 记录），再导入 ACM：

```bash
# 前提：已安装 acme.sh，并配置好 DNS 服务商的 API 凭证
DNS_PROVIDER=dns_namesilo ./apps/moxie/infra/issue-cert.sh moxie.example.com
```

为什么要绕这一圈：部分 DNS 服务商的 CNAME 记录拒绝以下划线开头的值，而 ACM 自带的 DNS 验证要求的正是 `_xxxx.acm-validations.aws.` 这样的 CNAME，可能导致验证永远卡在 PENDING。Let's Encrypt 用 TXT 记录，兼容性更好。

### 2. 配置 DNS

在域名服务商处添加 CNAME：

```
moxie.example.com  →  <distribution>.cloudfront.net
```

### 3. 更新参数并重新部署

编辑 `apps/moxie/infra/params.json`：

```json
{
  "DomainName": "moxie.example.com",
  "AcmCertificateArn": "arn:aws:acm:us-east-1:<account>:certificate/<id>"
}
```

两个参数任一为空时，模板自动退回 CloudFront 默认域名（`HasCustomDomain` 条件）。

```bash
cd apps/moxie/infra && ./deploy.sh dev
```

### 4. 证书自动续期

acme.sh 会通过 cron 自动续期，但 **CloudFront 使用的是 ACM 里导入的副本**，两者不会自动同步。只续期不重新导入，CloudFront 到期就会断 HTTPS。

在 acme.sh 上挂 reload 钩子：

```bash
~/.acme.sh/acme.sh --install-cert -d moxie.example.com \
  --reloadcmd "DOMAIN=moxie.example.com DIST_ID=<distribution-id> bash /path/to/moxie/apps/moxie/infra/renew-acm.sh"
```

`renew-acm.sh` 会：导入新证书到 ACM（us-east-1）→ 更新 CloudFront ViewerCertificate → 把新 ARN 同步回 `params.json`，避免下次 `deploy.sh` 覆盖冲突。

验证钩子存在：

```bash
crontab -l | grep acme
grep Le_ReloadCmd ~/.acme.sh/moxie.example.com_ecc/moxie.example.com.conf
```

## 六、日常运维

### 重新部署（改代码后）

```bash
cargo test --workspace        # 测试全绿再部署
cd apps/moxie/infra
./deploy.sh dev
```

只改前端时重跑 `deploy.sh` 即可（第 6 步会重传 HTML）。

### 看日志

```bash
aws logs tail /aws/lambda/moxie-dev-api --follow --format short
```

建议设置 7 天保留期，避免日志长期累积：

```bash
aws logs put-retention-policy \
  --log-group-name /aws/lambda/moxie-dev-api \
  --retention-in-days 7
```

### 轮换密钥

```bash
# 智谱 key
aws ssm put-parameter --name /moxie/dev/zhipu_api_key \
  --type SecureString --value "<新key>" --overwrite

# JWT 种子（轮换后所有已登录家长都需要重新登录）
aws ssm put-parameter --name /moxie/dev/jwt_seed \
  --type SecureString --value "<新seed>" --overwrite
```

### 删除整套环境

```bash
aws s3 rm s3://moxie-dev-frontend --recursive
aws s3 rm s3://moxie-dev-audio --recursive
aws cloudformation delete-stack --stack-name moxie-dev
aws s3 rb s3://moxie-deploy-<acct>-us-west-2 --force   # 部署桶在栈外，需手动删
```

> 前端桶开了版本控制，删除前要连同历史版本一起清空（或用生命周期规则），
> 否则 `delete-stack` 可能卡在 `DELETE_FAILED`。

## 七、部署排坑速查

| 现象 | 原因 / 处理 |
|---|---|
| 访问 `/api/...` 403 | 缺少 `x-amz-content-sha256`；或 CloudFront OAC 的两条 Lambda 权限缺一 |
| 自定义头丢失 | `/api/*` 没挂 `AllViewerExceptHostHeader` OriginRequestPolicy |
| 上传图片 413 | 图片未压缩；CloudFront 请求体上限约 1.5–2MB |
| `/health` 走 CloudFront 403 | CloudFront 只路由 `/api/*`，裸路径落到 S3；用 `/api/health` |
| Lambda 报 `GLIBC_2.xx not found` | 没有用 musl 静态链接编译 |
| 首次下发偶发失败 | Polly 瞬时 `service error`；代码已内置 3 次指数退避 |
| OCR 总是不可用 | 未配置 `zhipu_api_key`，且账号未开通 Bedrock |
| `deploy.sh` 报凭证未设置 | 脚本 `ROOT` 路径或 `.env` 位置不对；应在仓库根目录放 `.env` |
| `delete-stack` 卡住 | S3 桶非空 / 有历史版本 |
| 自定义域名 HTTPS 过期 | ACM 副本未随 acme.sh 续期更新，跑 `renew-acm.sh` 或检查钩子 |

更多实施中发现的问题见 [常见问题](faq.md) 与 [技术设计](design.md#八实施后记实测发现与设计偏离)。