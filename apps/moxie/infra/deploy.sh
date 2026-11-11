#!/usr/bin/env bash
# moxie 端到端部署 —— 独立 CloudFormation 栈 `moxie-{env}`
#
# 用法: ./deploy.sh [environment]   (默认 dev)
# 流程: 确保 SSM 密钥 → musl 编译 → 打包 bootstrap.zip → 上传 Lambda 代码
#       → CloudFormation deploy → 上传前端(S3) → 打印输出
#
# 前置条件:
#   - AWS 凭证（环境变量或 ~/.aws/credentials）与目标账号/区域
#   - cargo + rustup（脚本会自动尝试安装 musl target）
#   - zip、python3、awscli v2
#   - 首次部署前按 docs/deployment.md 创建 Lambda Web Adapter layer 的权限
#
# 若所在网络需要代理访问 AWS，可在项目根 .env 里设置：
#   PROXY_SCRIPT=$HOME/proxy.sh
# 脚本会在需要时 source 它（文件不存在则跳过）。
set -euo pipefail

# ---- 可选代理 ----
if [ -n "${PROXY_SCRIPT:-}" ] && [ -f "${PROXY_SCRIPT}" ]; then
  # shellcheck disable=SC1090
  source "${PROXY_SCRIPT}"
fi

# 脚本在 apps/moxie/infra/，项目根要往上三级：infra → moxie → apps → <repo-root>
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
if [ -f "$ROOT/.env" ]; then
  set -a; source "$ROOT/.env"; set +a
fi
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:?AWS_ACCESS_KEY_ID 未设置：复制 .env.example 为 .env 并填入凭证}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:?AWS_SECRET_ACCESS_KEY 未设置}"
export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-west-2}"

PROJECT="${OPERON_PROJECT:-moxie}"
ENV="${1:-dev}"
REGION="${AWS_DEFAULT_REGION}"
ACCOUNT_ID="$(aws sts get-caller-identity --query Account --output text)"
STACK_NAME="${PROJECT}-${ENV}"
BUCKET="${PROJECT}-deploy-${ACCOUNT_ID}-${REGION}"
FRONTEND_BUCKET="${PROJECT}-${ENV}-frontend"
AUDIO_BUCKET="${PROJECT}-${ENV}-audio"
SECRETS_PATH="/${PROJECT}/${ENV}/"
PACKAGE="operon-moxie"
TEMPLATE="apps/moxie/infra/template.yaml"
PARAMS="apps/moxie/infra/params.json"

cd "$ROOT"

step() { echo -e "\n====> $1"; }

step "[1/7] 确保 SSM 密钥存在（SecureString）"
# jwt_seed：签发家长登录态。密码用 argon2 存在 DynamoDB，无内置管理员密码。
if aws ssm get-parameter --name "${SECRETS_PATH}jwt_seed" --with-decryption --query Parameter.Value --output text >/dev/null 2>&1; then
  echo "      jwt_seed 已存在"
else
  SEED="$(openssl rand -base64 48)"
  aws ssm put-parameter --name "${SECRETS_PATH}jwt_seed" --type SecureString --value "$SEED" >/dev/null
  echo "      jwt_seed 已创建"
fi

# zhipu_api_key：拍照识别词表（GLM-4V）用。没设就跳过——粘贴文本录入不受影响。
if aws ssm get-parameter --name "${SECRETS_PATH}zhipu_api_key" --with-decryption --query Parameter.Value --output text >/dev/null 2>&1; then
  echo "      zhipu_api_key 已存在"
elif [ -n "${ZHIPU_API_KEY:-}" ]; then
  aws ssm put-parameter --name "${SECRETS_PATH}zhipu_api_key" --type SecureString --value "$ZHIPU_API_KEY" >/dev/null
  echo "      zhipu_api_key 已创建（OCR 可用）"
else
  echo "      zhipu_api_key 未配置 —— 拍照识别将不可用，可在 .env 里设 ZHIPU_API_KEY 后重跑"
fi

step "[2/7] 编译 release（$PACKAGE，musl 静态链接）"
# 必须 musl：Lambda AL2023 的 glibc 版本可能低于构建机，动态链接会 GLIBC_2.xx not found
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo build --release --target x86_64-unknown-linux-musl -p "$PACKAGE"

step "[3/7] 打包 bootstrap.zip"
ZIP="$ROOT/target/bootstrap-moxie-musl.zip"
rm -f "$ZIP"
PKG_DIR="$(mktemp -d)"
# provided.al2023 约定：可执行文件必须叫 bootstrap
cp "$ROOT/target/x86_64-unknown-linux-musl/release/${PACKAGE}" "$PKG_DIR/bootstrap"
( cd "$PKG_DIR" && zip -q "$ZIP" bootstrap )
rm -rf "$PKG_DIR"
ls -lh "$ZIP"

step "[4/7] 上传 Lambda 代码到 s3://${BUCKET}"
if ! aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
  aws s3api create-bucket --bucket "$BUCKET" --region "$REGION" \
    --create-bucket-configuration LocationConstraint="$REGION" >/dev/null
  echo "      已创建部署桶 $BUCKET"
fi
CODE_KEY="${PROJECT}/code/$(date +%s)/bootstrap.zip"
aws s3api put-object --bucket "$BUCKET" --key "$CODE_KEY" --body "$ZIP" \
  --cli-read-timeout 290 --cli-connect-timeout 30 >/dev/null
echo "      已上传 $CODE_KEY"

step "[5/7] CloudFormation 部署 $STACK_NAME"
# 从 apps/moxie/infra/params.json 读参数；CodeKey / CodeBucket 动态生成，
# 可用环境变量覆盖单个参数（OPERON_PROJECT / OPERON_ENV）
PARAM_OVERRIDES=$(python3 - "$CODE_KEY" "$BUCKET" <<'PYEOF'
import json, os, sys
code_key, code_bucket = sys.argv[1], sys.argv[2]
p = json.load(open('apps/moxie/infra/params.json'))
p['CodeKey'] = code_key
p['CodeBucket'] = code_bucket  # 用实际账号动态覆盖，避免 params.json 里的账号与实际不符
for k, env in {'Project': 'OPERON_PROJECT', 'Environment': 'OPERON_ENV'}.items():
    if os.environ.get(env):
        p[k] = os.environ[env]
for k, v in p.items():
    if v != "":
        print(f"{k}={v}")
PYEOF
)
aws cloudformation deploy \
  --stack-name "$STACK_NAME" \
  --template-file "$TEMPLATE" \
  --parameter-overrides $PARAM_OVERRIDES \
  --capabilities CAPABILITY_NAMED_IAM \
  --no-fail-on-empty-changeset

step "[6/7] 上传前端静态文件到 s3://${FRONTEND_BUCKET}"
FRONTEND_DIR="$ROOT/apps/moxie/frontend"
for f in parent.html child.html; do
  if [ -f "$FRONTEND_DIR/$f" ]; then
    aws s3api put-object --bucket "$FRONTEND_BUCKET" --key "$f" \
      --body "$FRONTEND_DIR/$f" --content-type "text/html; charset=utf-8" >/dev/null
    echo "      已上传 $f"
  else
    echo "      跳过（缺少 $FRONTEND_DIR/$f）"
  fi
done
# 音频桶不需要上传任何文件：MP3 由 Lambda 运行时写入（内容寻址，不可变）
echo "      音频桶 ${AUDIO_BUCKET} 由 Lambda 运行时写入，无需上传"

step "[7/7] 完成"
aws cloudformation describe-stacks --stack-name "$STACK_NAME" \
  --query 'Stacks[0].Outputs' --output table
CF_URL="$(aws cloudformation describe-stacks --stack-name "$STACK_NAME" \
  --query "Stacks[0].Outputs[?OutputKey=='CloudFrontUrl'].OutputValue" --output text)"
SITE_URL="$(aws cloudformation describe-stacks --stack-name "$STACK_NAME" \
  --query "Stacks[0].Outputs[?OutputKey=='SiteUrl'].OutputValue" --output text)"
echo
echo "站点:    ${SITE_URL}"
echo "孩子端:  ${SITE_URL}/child.html"
echo "家长端:  ${SITE_URL}/parent.html"
echo "API:     ${SITE_URL}/api/*"
echo "音频:    ${SITE_URL}/audio/*"
if [ "$SITE_URL" != "$CF_URL" ]; then
  echo
  echo "（CloudFront 默认域名仍可用：${CF_URL}）"
fi