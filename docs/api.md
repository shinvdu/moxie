# API 参考

所有接口经由 CloudFront 同源访问，前端使用相对路径 `/api/...`，无需 CORS。

## 通用约定

### 请求头

| 头 | 何时需要 | 说明 |
|---|---|---|
| `Content-Type: application/json` | 有 JSON body 时 | |
| `x-amz-content-sha256` | **所有带 body 的请求** | body 的 SHA-256 十六进制；CloudFront / Lambda Function URL 校验，缺失会 403 |
| `X-Authorization: Bearer <jwt>` | 家长端 | 不用标准 `Authorization`——CloudFront OAC 会占用它做 SigV4 签名 |
| `X-Device-Token: <token>` | 孩子端 | 绑定后获得的长期设备令牌 |

`x-amz-content-sha256` 在前端用浏览器 API 计算：

```javascript
const body = JSON.stringify(payload);
const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(body));
const hash = [...new Uint8Array(digest)].map(b => b.toString(16).padStart(2, '0')).join('');
await fetch('/api/...', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json', 'x-amz-content-sha256': hash },
  body,
});
```

### 错误格式

```json
{
  "error": {
    "code": "BAD_REQUEST",
    "message": "词表为空"
  }
}
```

| HTTP | code | 典型场景 |
|---|---|---|
| 400 | `BAD_REQUEST` | 参数非法、绑定码无效或过期、词表为空、图片解码失败 |
| 401 | `UNAUTHORIZED` | 缺少 / 无效 JWT、缺少 / 无效设备令牌、密码错误 |
| 403 | `FORBIDDEN` | 缺少 `x-amz-content-sha256` 等网关校验失败 |
| 404 | `NOT_FOUND` | 孩子不存在 / 不属于当前账号 / 尚未下发 |
| 409 | `CONFLICT` | 手机号已注册 |
| 500 | `INTERNAL` | Polly / OCR / 数据库等内部错误 |

### 认证模型

- **家长 JWT**：Ed25519 签发，有效期 30 天，`extra.role = "parent"`，`sub` 为 `user_id`；
- **设备令牌**：`{user_id}.{kid_id}.{64 位随机 hex}`，不透明随机串，服务端只存 SHA-256。选择不透明令牌而非 JWT，是因为**家长解绑后需要立刻失效**。

---

## 公开接口

### `GET /api/health`

健康检查。`/health` 也可用，但只对直连 Lambda Function URL 有效（CloudFront 只路由 `/api/*`）。

```json
{ "status": "ok", "service": "operon-moxie" }
```

---

### `POST /api/auth/register`

注册家长账号。

```json
{
  "phone": "13800138000",
  "password": "至少6位",
  "nickname": "可选"
}
```

响应 `201`：

```json
{
  "token": "<jwt>",
  "user_id": "u_3f9a...",
  "nickname": "可选"
}
```

约束：手机号为 11 位数字且以 1 开头；密码 6–128 位；手机号已存在返回 `409`。

---

### `POST /api/auth/login`

```json
{ "phone": "13800138000", "password": "..." }
```

响应 `200`：同注册。手机号不存在与密码错误统一返回 `401 手机号或密码不正确`，避免泄露账号是否存在。

---

### `POST /api/kid/bind`

孩子平板用一次性绑定码换取长期设备令牌。**公开接口**，无需任何登录态。

```json
{ "code": "G926KX" }
```

响应 `200`：

```json
{ "device_token": "u_...k_....<64hex>", "kid_name": "小明" }
```

绑定码特性：6 位、去掉了易混字符 `0/O/1/I/L`、5 分钟有效、**无论成功与否都立即删除**（防重放）。返回的令牌由前端存在 `localStorage`。

---

## 家长端（JWT）

所有 `{kid_id}` 操作都会先校验该孩子属于当前 JWT 的 `user_id`；不属于自己的一律 `404`，不区分「不存在」与「不属于你」。

### `GET /api/kids`

列出当前账号下的孩子，按创建时间升序。

```json
[
  {
    "kid_id": "k_2b7c...",
    "name": "小明",
    "grade": 4,
    "bound": true,
    "created_at": 1786701600
  }
]
```

`bound` 表示是否已绑定平板；响应不含设备令牌哈希。

---

### `POST /api/kids`

```json
{ "name": "小明", "grade": 4 }
```

响应 `201`：单个 `KidOut`。姓名 1–20 字，年级 1–12（可省略）。

---

### `POST /api/kids/{kid_id}/bind-code`

生成一次性绑定码。

```json
{ "code": "G926KX", "expires_in": 300 }
```

---

### `DELETE /api/kids/{kid_id}/device`

解绑平板：清空 `device_token_hash`。旧设备令牌**立即失效**。

响应 `204 No Content`。

---

### `GET /api/kids/{kid_id}/words`

家长端查看当前词表（**含汉字与拼音**，用于编辑和试听）。

```json
{
  "title": "第2课《繁星》",
  "pushed_at": 1786701600,
  "default_repeat": 1,
  "words": [
    {
      "text": "模糊",
      "pinyin": "mo2 hu2",
      "a1": "/audio/1a2b3c4d5e6f7a8b.mp3",
      "a2": "/audio/9f8e7d6c5b4a3210.mp3"
    }
  ]
}
```

尚未下发时返回 `words: []`、`default_repeat: 1`。

---

### `POST /api/kids/{kid_id}/push`

下发词表：确保每个词的两个音频版本（念一遍 / 念两遍）都存在，然后写入当前词表。

```json
{
  "title": "第2课《繁星》",
  "default_repeat": 1,
  "words": [
    { "text": "繁星" },
    { "text": "模糊", "pinyin": "mo2 hu2" },
    { "text": "糊涂", "pinyin": "hu2 tu0" }
  ]
}
```

- `title` 可选，**只给家长看，不会下发给孩子端**；
- `default_repeat` 为 `1` 或 `2`；大于 1 按 2 处理；
- `pinyin` 可选，格式为数字声调 + 空格分隔音节（1 阴平 / 2 阳平 / 3 上声 / 4 去声 / 0 轻声）；
- 词语会 trim、去重；每词最多 30 字；一次最多 200 词。

响应 `200`：

```json
{ "generated": 0, "cached": 38, "total": 38 }
```

`total` 是音频段数（词数 × 2）。`generated + cached == total`。重复下发同一词表通常 `generated: 0`。

---

### `POST /api/preview`

试听单个词，必要时生成音频。

```json
{ "text": "模糊", "pinyin": "mo2 hu2" }
```

响应 `200`：

```json
{ "a1": "/audio/....mp3", "a2": "/audio/....mp3" }
```

---

### `POST /api/ocr`

拍照识别词表。

```json
{
  "image_base64": "<base64，可带 data:image/jpeg;base64, 前缀>",
  "media_type": "image/jpeg"
}
```

响应 `200`：

```json
{ "words": ["繁星", "繁华", "怀抱", "怀旧"] }
```

- 图片必须在浏览器端压缩到长边 ≤1600px（CloudFront 请求体上限约 1.5–2MB）；
- 解码后体积上限 4MB；
- 默认走 GLM-4V（需 SSM `zhipu_api_key`）；配置 `OPERON_MOXIE_BEDROCK_MODEL_ID` 时优先尝试 Bedrock；
- 解析时会丢掉拼音、编号、英文说明，只保留含汉字的 token。

---

## 孩子端（设备令牌）

### `GET /api/kid/current`

获取当前默写会话。**响应中没有任何汉字**——这是防偷看的核心保证。

```json
{
  "total": 19,
  "default_repeat": 1,
  "items": [
    { "a1": "/audio/....mp3", "a2": "/audio/....mp3" },
    { "a1": "/audio/....mp3", "a2": "/audio/....mp3" }
  ]
}
```

刻意不下发 `title`：标题「第2课《繁星》」里的「繁星」可能就是待默写的词。

错误：

| 状态 | 场景 |
|---|---|
| 401 | 缺少设备令牌 / 格式错误 / 已解绑 |
| 404 | 家长尚未下发词表 |

---

### `GET /api/kid/answers`

默写结束后取答案，供核对。

```json
{
  "title": "第2课《繁星》",
  "words": ["繁星", "繁华", "怀抱"]
}
```

---

## curl 示例

```bash
BASE=https://your-distribution.cloudfront.net

# 注册
BODY='{"phone":"13800138000","password":"test1234"}'
HASH=$(printf '%s' "$BODY" | sha256sum | cut -d' ' -f1)
curl -s "$BASE/api/auth/register" \
  -H 'Content-Type: application/json' \
  -H "x-amz-content-sha256: $HASH" \
  -d "$BODY"

# 带 JWT 的请求
curl -s "$BASE/api/kids" -H "X-Authorization: Bearer $TOKEN"
```