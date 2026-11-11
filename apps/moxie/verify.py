#!/usr/bin/env python3
"""moxie 部署后的端到端验证。

用法:
    python3 verify.py https://<cloudfront-domain>

覆盖：
  1. 注册 / 登录
  2. 新建孩子
  3. 下发第2课《繁星》20 个词 → 触发 Polly 生成音频
  4. 内容寻址：重复下发应全部命中缓存（不重复调 Polly）
  5. 绑定码 → 设备令牌
  6. 孩子端会话**不含任何汉字**（防偷看，含标题）
  7. 音频经 CloudFront 可取且 Content-Type 正确
  8. 越权：用 A 的令牌访问 B 的孩子必须 404
  9. 无密钥下发必须 403/401
"""

import hashlib
import json
import random
import string
import sys
import urllib.error
import urllib.request

BASE = sys.argv[1].rstrip("/") if len(sys.argv) > 1 else None
if not BASE:
    print("用法: python3 verify.py https://<cloudfront-domain>")
    sys.exit(2)

# 第2课《繁星》—— 图片识别结果
WORDS = [
    "繁星", "繁华", "怀抱", "怀旧", "寂寞", "寂静", "谈吐", "交谈",
    "成熟", "熟悉", "平躺", "躺椅", "模仿", "模糊", "糊涂", "围巾",
    "周围", "舞蹈", "跳舞",
]

PASS, FAIL = [], []


def check(name, ok, detail=""):
    (PASS if ok else FAIL).append(name)
    print(f"  {'✓' if ok else '✗'} {name}" + (f"  —— {detail}" if detail else ""))


def call(method, path, body=None, token=None, device_token=None, raw=False):
    """返回 (status, data)。raw=True 时 data 是字节。"""
    url = BASE + path
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
        # Lambda Function URL 不支持 unsigned payload，POST 必须带这个头
        headers["x-amz-content-sha256"] = hashlib.sha256(data).hexdigest()
    if token:
        headers["X-Authorization"] = f"Bearer {token}"
    if device_token:
        headers["X-Device-Token"] = device_token

    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=90) as r:
            payload = r.read()
            ct = r.headers.get("Content-Type", "")
            if raw:
                return r.status, payload, ct
            return r.status, (json.loads(payload) if payload else None)
    except urllib.error.HTTPError as e:
        payload = e.read()
        if raw:
            return e.code, payload, ""
        try:
            return e.code, json.loads(payload)
        except Exception:
            return e.code, {"raw": payload.decode(errors="replace")[:200]}


def rnd_phone():
    return "1" + "".join(random.choice(string.digits) for _ in range(10))


def has_cjk(s):
    return any("一" <= c <= "鿿" for c in s)


def main():
    print(f"\n目标: {BASE}\n")

    # ---- 0. 健康检查 ----
    print("[0] 健康检查")
    # 注意走 /api/health：CloudFront 只把 /api/* 路由到 Lambda，
    # 裸 /health 会落到 S3 前端桶返回 403（该路由留给直连 Function URL 用）
    st, d = call("GET", "/api/health")
    check("GET /api/health 返回 200", st == 200, f"status={st} body={d}")

    # ---- 1. 注册 ----
    print("\n[1] 注册 / 登录")
    phone = rnd_phone()
    st, d = call("POST", "/api/auth/register",
                 {"phone": phone, "password": "test1234", "nickname": "验证账号"})
    check("注册返回 201", st == 201, f"status={st} body={d}")
    if st != 201:
        print("\n注册失败，后续无法继续")
        return
    token = d["token"]

    st, d = call("POST", "/api/auth/login", {"phone": phone, "password": "test1234"})
    check("用同一手机号能登录", st == 200, f"status={st}")

    st, d = call("POST", "/api/auth/login", {"phone": phone, "password": "wrongpass"})
    check("错误密码返回 401", st == 401, f"status={st}")

    st, d = call("POST", "/api/auth/register", {"phone": phone, "password": "test1234"})
    check("重复注册返回 409", st == 409, f"status={st}")

    # ---- 2. 新建孩子 ----
    print("\n[2] 新建孩子")
    st, kid = call("POST", "/api/kids", {"name": "小明", "grade": 4}, token=token)
    check("新建孩子返回 201", st == 201, f"status={st} body={kid}")
    if st != 201:
        return
    kid_id = kid["kid_id"]
    check("新孩子默认未绑定", kid.get("bound") is False)
    check("孩子信息不含设备令牌哈希", "device_token_hash" not in json.dumps(kid))

    # ---- 3. 下发词表 ----
    print("\n[3] 下发第2课《繁星》（触发 Polly 生成）")
    st, push = call("POST", f"/api/kids/{kid_id}/push",
                    {"title": "第2课《繁星》",
                     "words": [{"text": w} for w in WORDS],
                     "default_repeat": 1},
                    token=token)
    check("下发返回 200", st == 200, f"status={st} body={push}")
    if st != 200:
        return
    total = push["total"]
    check("音频总数 = 词数 × 2 = 38", total == len(WORDS) * 2, f"total={total}")
    # 注意：不要断言 generated > 0。音频是内容寻址、**跨用户跨课文共享**的，
    # 重复跑本脚本（或别的家庭学过同一课）时 generated 会是 0，全部命中缓存——
    # 这正是设计目标，不是失败。
    check("生成数 + 复用数 = 总数",
          push["generated"] + push["cached"] == total,
          f"generated={push['generated']} cached={push['cached']} total={total}")
    print(f"      新生成 {push['generated']} 段，复用 {push['cached']} 段"
          f"（复用=跨用户共享的教材音频）")

    # ---- 4. 内容寻址：重复下发应全命中 ----
    print("\n[4] 内容寻址（重复下发同一词表）")
    st, push2 = call("POST", f"/api/kids/{kid_id}/push",
                     {"title": "第2课《繁星》",
                      "words": [{"text": w} for w in WORDS],
                      "default_repeat": 1},
                     token=token)
    check("重复下发成功", st == 200, f"status={st}")
    if st == 200:
        check("第二次全部命中缓存，不重复调 Polly",
              push2["generated"] == 0 and push2["cached"] == total,
              f"generated={push2['generated']} cached={push2['cached']}")

    # ---- 5. 绑定码 → 设备令牌 ----
    print("\n[5] 绑定平板")
    st, bc = call("POST", f"/api/kids/{kid_id}/bind-code", {}, token=token)
    check("生成绑定码返回 200", st == 200, f"status={st} body={bc}")
    if st != 200:
        return
    code = bc["code"]
    check("绑定码为 6 位", len(code) == 6, f"code={code}")
    check("绑定码不含易混字符 0/O/1/I/L",
          not any(c in code for c in "0O1IL"), f"code={code}")

    st, bd = call("POST", "/api/kid/bind", {"code": code})
    check("绑定返回 200", st == 200, f"status={st} body={bd}")
    if st != 200:
        return
    dev = bd["device_token"]
    check("拿到设备令牌", bool(dev))

    st, _ = call("POST", "/api/kid/bind", {"code": code})
    check("绑定码用完即废（重放返回 400）", st == 400, f"status={st}")

    # ---- 6. 孩子端会话：绝对不能有汉字 ----
    print("\n[6] 孩子端会话（防偷看）")
    st, sess = call("GET", "/api/kid/current", device_token=dev)
    check("孩子端取会话返回 200", st == 200, f"status={st} body={sess}")
    if st == 200:
        blob = json.dumps(sess, ensure_ascii=False)
        check("会话里没有 title 字段", "title" not in sess)
        check("会话里没有 text 字段", "text" not in blob)
        check("会话里没有 pinyin 字段", "pinyin" not in blob)
        check("会话里没有任何汉字", not has_cjk(blob), f"泄露内容: {blob[:200]}")
        check("总数正确", sess["total"] == len(WORDS), f"total={sess['total']}")
        for w in WORDS:
            check(f"不泄露词语「{w}」", w not in blob)
        items = sess["items"]
        a1 = items[0]["a1"]
        a2 = items[0]["a2"]
        check("a1/a2 是音频路径", a1.startswith("/audio/") and a2.startswith("/audio/"))
        check("念一遍与念两遍是不同文件", a1 != a2)

    st, ans = call("GET", "/api/kid/answers", device_token=dev)
    check("答案接口能取到词（默写后才调）", st == 200 and len(ans.get("words", [])) == len(WORDS),
          f"status={st}")

    # ---- 7. 音频可取 ----
    print("\n[7] 音频经 CloudFront 分发")
    st, payload, ct = call("GET", a1, raw=True)
    check("音频返回 200", st == 200, f"status={st}")
    check("Content-Type 是 audio/mpeg", "audio/mpeg" in ct, f"ct={ct}")
    check("是 MP3（ID3 或帧同步头）",
          payload[:3] == b"ID3" or (len(payload) > 1 and payload[0] == 0xFF),
          f"前几字节={payload[:4]!r}")
    check("音频非空", len(payload) > 500, f"size={len(payload)}")

    # ---- 8. 越权 ----
    print("\n[8] 越权防护")
    phone2 = rnd_phone()
    st, d2 = call("POST", "/api/auth/register",
                  {"phone": phone2, "password": "test1234"})
    other_token = d2["token"] if st == 201 else None
    if other_token:
        st, _ = call("GET", f"/api/kids/{kid_id}/words", token=other_token)
        check("他人令牌访问该孩子返回 404", st == 404, f"status={st}")
        st, _ = call("POST", f"/api/kids/{kid_id}/push",
                     {"words": [{"text": "繁星"}]}, token=other_token)
        check("他人令牌下发到该孩子被拒", st == 404, f"status={st}")
        st, _ = call("POST", f"/api/kids/{kid_id}/bind-code", {}, token=other_token)
        check("他人令牌生成绑定码被拒", st == 404, f"status={st}")

    # ---- 9. 无凭证 ----
    print("\n[9] 无凭证访问")
    st, _ = call("GET", f"/api/kids/{kid_id}/words")
    check("无 JWT 取词表返回 401", st == 401, f"status={st}")
    st, _ = call("GET", "/api/kid/current")
    check("无设备令牌取会话返回 401", st == 401, f"status={st}")
    st, _ = call("GET", "/api/kid/current", device_token="u_fake.k_fake." + "a" * 64)
    check("伪造设备令牌返回 401/404", st in (401, 404), f"status={st}")

    # ---- 10. 解绑后令牌失效 ----
    print("\n[10] 解绑设备")
    st, _ = call("DELETE", f"/api/kids/{kid_id}/device", token=token)
    check("解绑返回 204", st == 204, f"status={st}")
    st, _ = call("GET", "/api/kid/current", device_token=dev)
    check("解绑后旧设备令牌立即失效", st == 401, f"status={st}")

    # ---- 汇总 ----
    print("\n" + "=" * 56)
    print(f"通过 {len(PASS)} 项，失败 {len(FAIL)} 项")
    if FAIL:
        print("\n失败项：")
        for f in FAIL:
            print(f"  ✗ {f}")
    print("=" * 56)
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
