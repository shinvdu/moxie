# 样例数据

这些样例来自第 2 课《繁星》的真实使用场景，可直接用于测试和演示。

## words/

| 文件 | 说明 |
|---|---|
| `fanxing.json` | 19 个去重后的词语，不带拼音（Polly 自判） |
| `fanxing-pinyin.json` | 同上，但为 5 个多音字风险词补上拼音 SSML，作为「家长纠错」示例 |

两个文件都符合 `POST /api/kids/{kid_id}/push` 的请求体格式：

```bash
BODY=$(cat samples/words/fanxing.json)
HASH=$(printf '%s' "$BODY" | sha256sum | cut -d' ' -f1)
curl -X POST "$BASE/api/kids/$KID_ID/push" \
  -H 'Content-Type: application/json' \
  -H "x-amz-content-sha256: $HASH" \
  -H "X-Authorization: Bearer $TOKEN" \
  -d "$BODY"
```

声调数字：1 阴平 / 2 阳平 / 3 上声 / 4 去声 / 0 轻声。

## ocr/

| 文件 | 说明 |
|---|---|
| `word-list-photo.jpg` | 老师发的生字表照片（压缩到 1600px，与前端上传前一致） |
| `expected-words.txt` | 期望 OCR 输出（19 个去重词语） |

可用于验证 `POST /api/ocr` 和 `glm-4v-plus` 的识别准确率。原始生字表共 20 个词，其中「模糊」出现两次，去重后 19 个。

## pronunciation/

发音校对样本，由 AWS Polly 在项目实施阶段生成，用于人工试听核对多音字。详见 [pronunciation/README.md](pronunciation/README.md)。

> 说明：这些音频仅作为发音校对样例随项目提供，不构成对 AWS Polly 输出内容的再授权；实际部署时请按 AWS 服务条款使用。