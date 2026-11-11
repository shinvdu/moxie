#!/usr/bin/env bash
# 为 moxie 的自定义域名签发证书并导入 ACM（us-east-1）。
#
# 用法:
#   DNS_PROVIDER=dns_namesilo ./issue-cert.sh moxie.example.com
#
# 前置条件:
#   - 已安装 acme.sh: curl -s https://get.acme.sh | sh
#   - 已按 acme.sh 文档配置好所选 DNS 服务商的 API 凭证
#     （例如 NameSilo 的 Namesilo_Key、Cloudflare 的 CF_Token 等）
#   - 已配置 AWS 凭证，且 python3 + boto3 可用
#
# ## 为什么用 Let's Encrypt（DNS-01）而不是 ACM 自带的 DNS 验证
#
# 部分 DNS 服务商（如 NameSilo）的 CNAME 记录拒绝以下划线开头的值，而 ACM 的
# DNS 验证要求添加一条 `_xxxx.acm-validations.aws.` 的 CNAME，可能导致验证
# 永远卡在 PENDING。Let's Encrypt 的 DNS-01 用的是 **TXT** 记录
# （`_acme-challenge.<域名>`），provider 的 acme.sh hook 会自动增删。
#
# ## 为什么要导入 ACM
#
# CloudFront 只接受 ACM 证书，且**必须是 us-east-1 的**（全球边缘服务的硬性
# 要求，与栈所在的区域无关）。
#
# ## 续期
#
# acme.sh 的 cron 会自动续期。续期后需要重新导入 ACM 并更新 CloudFront，
# 用 `acme.sh --install-cert --reloadcmd` 挂 renew-acm.sh 即可。
# 本脚本只负责首次签发 + 导入。

set -euo pipefail

if [ $# -lt 1 ]; then
  echo "用法: DNS_PROVIDER=dns_xxx $0 <域名>" >&2
  exit 2
fi

DOMAIN="$1"
DNS_PROVIDER="${DNS_PROVIDER:?请设置 DNS_PROVIDER，例如 dns_namesilo / dns_cf / dns_ali}"
REGION_ACM="${ACM_REGION:-us-east-1}"   # CloudFront 证书必须 us-east-1

log() { echo "[issue-cert] $*"; }

# ---- 前置检查 ----
[ -x "$HOME/.acme.sh/acme.sh" ] || { log "acme.sh 未安装：curl -s https://get.acme.sh | sh"; exit 1; }

# ---- 1. 签发（DNS-01，TXT 记录由 acme.sh 自动增删）----
CERT_DIR="$HOME/.acme.sh/${DOMAIN}_ecc"
if [ -f "$CERT_DIR/${DOMAIN}.cer" ]; then
  log "证书已存在，执行续期"
  "$HOME/.acme.sh/acme.sh" --renew -d "$DOMAIN" --force --server letsencrypt
else
  log "签发新证书 $DOMAIN（DNS provider: $DNS_PROVIDER）"
  "$HOME/.acme.sh/acme.sh" --issue --dns "$DNS_PROVIDER" -d "$DOMAIN" --server letsencrypt
fi

[ -f "$CERT_DIR/${DOMAIN}.cer" ] || { log "签发失败，证书未生成"; exit 1; }

# ---- 2. 导入 ACM ----
# 注意：部分 AWS CLI 版本对 blob 参数有解析问题（Invalid base64）。
# 这里用 boto3 直接传 bytes，绕开该问题。
log "导入 ACM（$REGION_ACM）"
ARN=$(
  AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:?缺少 AWS_ACCESS_KEY_ID}" \
  AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:?缺少 AWS_SECRET_ACCESS_KEY}" \
  CERT_DIR="$CERT_DIR" DOMAIN="$DOMAIN" ACM_REGION="$REGION_ACM" python3 - <<'PY'
import boto3, os
d, domain, region = os.environ["CERT_DIR"], os.environ["DOMAIN"], os.environ["ACM_REGION"]
acm = boto3.client("acm", region_name=region)
arn = acm.import_certificate(
    Certificate=open(f"{d}/{domain}.cer", "rb").read(),
    PrivateKey=open(f"{d}/{domain}.key", "rb").read(),
    CertificateChain=open(f"{d}/ca.cer", "rb").read(),
)["CertificateArn"]
print(arn)
PY
)

log "证书 ARN: $ARN"
log "到期时间:"
AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID}" AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY}" \
  aws acm describe-certificate --region "$REGION_ACM" --certificate-arn "$ARN" \
  --query 'Certificate.{Domain:DomainName,Status:Status,NotAfter:NotAfter}' --output table

cat <<EOF

====> 下一步
1. 把证书 ARN 写进 apps/moxie/infra/params.json 的 AcmCertificateArn
2. 把 DomainName 写进 params.json，并在 DNS 服务商处添加 CNAME 指向 CloudFront 域名
3. 重新部署：cd apps/moxie/infra && ./deploy.sh dev
EOF