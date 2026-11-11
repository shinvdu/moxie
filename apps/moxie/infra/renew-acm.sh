#!/usr/bin/env bash
# 证书续期后的完整处理：
#   导入 ACM（us-east-1）→ 更新 CloudFront ViewerCertificate → 同步 params.json
#
# 由 acme.sh 在续期成功后自动调用：
#   ~/.acme.sh/acme.sh --install-cert -d moxie.example.com \
#     --reloadcmd "DOMAIN=moxie.example.com DIST_ID=EXXXXXXXXXXXXX bash /path/to/renew-acm.sh"
#
# ## 为什么需要这个脚本
#
# acme.sh 续期只更新 ~/.acme.sh/ 下的证书文件，而 **CloudFront 用的是 ACM 里
# 导入的那份副本**——两者不会自动同步。不挂这个钩子的话，Let's Encrypt 那边
# 续期成功了，CloudFront 上挂的还是旧证书，到期就断 HTTPS。
#
# 用法:
#   DOMAIN=moxie.example.com DIST_ID=EXXXXXXXXXXXXX ./renew-acm.sh

set -euo pipefail

# ---- 可选代理 ----
if [ -n "${PROXY_SCRIPT:-}" ] && [ -f "${PROXY_SCRIPT}" ]; then
  # shellcheck disable=SC1090
  source "${PROXY_SCRIPT}"
elif [ -f "$HOME/proxy.sh" ] && [ "${USE_HOME_PROXY:-0}" = "1" ]; then
  # shellcheck disable=SC1090
  source "$HOME/proxy.sh"
fi

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
if [ -f "$ROOT/.env" ]; then
  set -a; source "$ROOT/.env"; set +a
fi
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:?AWS_ACCESS_KEY_ID 未设置}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:?AWS_SECRET_ACCESS_KEY 未设置}"
export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-west-2}"

DOMAIN="${DOMAIN:?请设置 DOMAIN，例如 moxie.example.com}"
DIST_ID="${DIST_ID:?请设置 DIST_ID（CloudFront Distribution ID）}"
REGION_ACM="${ACM_REGION:-us-east-1}"          # CloudFront 证书必须 us-east-1
REGION_CF="${CLOUDFRONT_REGION:-us-east-1}"    # CloudFront API 的全局端点区域
CERT_DIR="$HOME/.acme.sh/${DOMAIN}_ecc"
PARAMS="$ROOT/apps/moxie/infra/params.json"

log() { echo "[renew-acm] $*"; }

[ -f "$CERT_DIR/${DOMAIN}.cer" ] || { log "证书不存在: $CERT_DIR（先跑 issue-cert.sh）"; exit 1; }

# ---------- 1. 导入 ACM ----------
log "导入 ACM…"
NEW_ARN=$(CERT_DIR="$CERT_DIR" DOMAIN="$DOMAIN" ACM_REGION="$REGION_ACM" python3 - <<'PY'
import boto3, os
d, domain, region = os.environ["CERT_DIR"], os.environ["DOMAIN"], os.environ["ACM_REGION"]
acm = boto3.client("acm", region_name=region)
print(acm.import_certificate(
    Certificate=open(f"{d}/{domain}.cer", "rb").read(),
    PrivateKey=open(f"{d}/{domain}.key", "rb").read(),
    CertificateChain=open(f"{d}/ca.cer", "rb").read(),
)["CertificateArn"])
PY
)
log "新证书 ARN: $NEW_ARN"

# ---------- 2. 更新 CloudFront ----------
# 直接改 distribution 比走 CloudFormation 快得多（秒级 vs 几分钟），
# 但下次 deploy.sh 会用 params.json 里的 ARN 覆盖回去，
# 所以第 3 步必须把新 ARN 写回 params.json，两者保持一致。
log "更新 CloudFront ViewerCertificate…"
DIST_ID="$DIST_ID" NEW_ARN="$NEW_ARN" REGION_CF="$REGION_CF" python3 - <<'PY'
import boto3, os
cf = boto3.client("cloudfront", region_name=os.environ["REGION_CF"])
did, arn = os.environ["DIST_ID"], os.environ["NEW_ARN"]
cfg = cf.get_distribution_config(Id=did)
conf, etag = cfg["DistributionConfig"], cfg["ETag"]
conf["ViewerCertificate"] = {
    "ACMCertificateArn": arn,
    "SSLSupportMethod": "sni-only",
    "MinimumProtocolVersion": "TLSv1.2_2021",
    "Certificate": arn,
    "CertificateSource": "acm",
}
cf.update_distribution(DistributionConfig=conf, Id=did, IfMatch=etag)
print("[renew-acm] CloudFront 已更新")
PY

# ---------- 3. 同步 params.json ----------
if [ -f "$PARAMS" ]; then
  log "同步 params.json…"
  PARAMS="$PARAMS" NEW_ARN="$NEW_ARN" DOMAIN="$DOMAIN" python3 - <<'PY'
import json, os
p = os.environ["PARAMS"]
d = json.load(open(p))
old = d.get("AcmCertificateArn", "")
d["AcmCertificateArn"] = os.environ["NEW_ARN"]
d["DomainName"] = os.environ["DOMAIN"]
json.dump(d, open(p, "w"), indent=2, ensure_ascii=False)
open(p, "a").write("\n")
print(f"[renew-acm] params.json: {old or '(空)'} → {os.environ['NEW_ARN']}")
PY
else
  log "未找到 params.json，跳过同步（若使用 CloudFormation 部署，请手动更新参数）"
fi

# ---------- 4. 完成 ----------
# 每次 import_certificate 都会生成新的 ARN，旧证书会留在 ACM 里。
# imported 证书本身不计费，可手动删除：
#   aws acm describe-certificate --region us-east-1 --certificate-arn <旧ARN>
#   aws acm delete-certificate   --region us-east-1 --certificate-arn <旧ARN>
log "完成。新证书 ARN: $NEW_ARN"
log "CloudFront 已直接更新（秒级生效），params.json 已同步，下次 deploy.sh 不会冲突。"