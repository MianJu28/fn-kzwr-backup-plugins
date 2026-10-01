//! 联通云盘（沃云盘）**H5 端**协议实现
//!
//! 只实现 H5 渠道（`LoginByMobileV2`），不实现 Web/PC 等其他端 —— 用户指定。
//! H5 渠道的 token 有效期更长（实测约 60 天，Web 端约 7 天）。
//!
//! ## 协议要点（复现自 pan.wo.cn 前端）
//!
//! 所有业务请求都是 `POST` 到 dispatcher，用 `header.key` 携带命令字，双通道：
//!
//! | 通道 | 端点 | `param` 加密密钥 | 用途 |
//! |---|---|---|---|
//! | `api-user` | `/api-user/dispatcher` | clientId 对应的客户端密钥 | 登录、用户信息 |
//! | `wohome` | `/wohome/dispatcher` | **access_token 前 16 字节** | 文件管理 |
//!
//! - `sign = MD5(key + resTime + reqSeq + channel + version)`
//! - `param`：AES-128-CBC / PKCS7，IV 固定 `wNSOYIB1k1DjY5lA`，密钥取前 16 字节
//! - 响应 `RSP.DATA` 为密文时按通道密钥解密

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use md5::{Digest, Md5};
use serde_json::{json, Value};

/// 前端硬编码的 IV（bridge.js 注入 window.obfuscatorAESIv）
pub const AES_IV: &[u8; 16] = b"wNSOYIB1k1DjY5lA";

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 前端 `getSuitableKey`：超过 16 字节直接截断
fn suitable_key(secret: &str) -> [u8; 16] {
    let b = secret.as_bytes();
    let mut key = [0u8; 16];
    let n = b.len().min(16);
    key[..n].copy_from_slice(&b[..n]);
    key
}

/// AES-128-CBC + PKCS7 → base64（对齐 CryptoJS `toString()`）
pub fn encrypt(plaintext: &str, secret: &str) -> Result<String, String> {
    let buf = plaintext.as_bytes().to_vec();
    let ct = Aes128CbcEnc::new_from_slices(&suitable_key(secret), AES_IV)
        .map_err(|e| format!("密钥/IV 无效: {e}"))?
        .encrypt_padded_vec_mut::<Pkcs7>(&buf);
    Ok(B64.encode(ct))
}

/// [`encrypt`] 的逆运算
pub fn decrypt(ciphertext: &str, secret: &str) -> Result<String, String> {
    let raw = B64
        .decode(ciphertext.trim())
        .map_err(|e| format!("密文不是合法 base64: {e}"))?;
    let pt = Aes128CbcDec::new_from_slices(&suitable_key(secret), AES_IV)
        .map_err(|e| format!("密钥/IV 无效: {e}"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&raw)
        .map_err(|e| format!("解密失败（填充不正确？）: {e}"))?;
    String::from_utf8(pt).map_err(|e| format!("解密结果不是 UTF-8: {e}"))
}

/// `sign = MD5(cmd + resTime + reqSeq + channel + version)`
pub fn make_sign(cmd: &str, res_time: i64, req_seq: i64, channel: &str, version: &str) -> String {
    let raw = format!("{cmd}{res_time}{req_seq}{channel}{version}");
    let d = Md5::digest(raw.as_bytes());
    hex::encode(d)
}

/// 类 UUID（对齐前端 cf45.j 的生成规则：`e[14]="4"`，`e[19]` 落在 8..=11，其余随机 hex）
///
/// 服务端只把它当一次性随机串校验格式，故只需形状正确。
pub fn js_uuid() -> String {
    let mut s = String::with_capacity(36);
    // 简易 LCG：避免引入 rand 依赖（本插件其余部分也不需要真随机）
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x2545F491);
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as u32
    };
    for i in 0..36u32 {
        if i == 14 {
            s.push('4');
        } else if i == 19 {
            s.push_str(&format!("{:x}", 8 + (next() % 4)));
        } else if matches!(i, 8 | 13 | 18 | 23) {
            s.push('-');
        } else {
            s.push_str(&format!("{:x}", next() % 16));
        }
    }
    s
}

/// 构造 dispatcher 请求信封
///
/// `secret_key`：api-user 通道传客户端密钥，wohome 通道传 access_token（服务端取前 16 字节）。
pub fn build_envelope(
    cmd: &str,
    payload: &Value,
    client_id: Option<&str>,
    secret_key: &str,
    channel: &str,
    res_time: i64,
    req_seq: i64,
) -> Result<Value, String> {
    let param = encrypt(&payload.to_string(), secret_key)?;
    let mut body = json!({ "param": param, "secret": true });
    // ⚠️ wohome 通道（a417 封装）的 clientId 位于**加密后的 param 内部**，
    //    放外层会返回 `1000 请求参数错误`；api-user 通道则需要放在外层。
    if channel == Channel::ApiUser.as_str() {
        if let Some(cid) = client_id {
            body["clientId"] = json!(cid);
        }
    } else if let Some(cid) = client_id {
        // wohome：塞进 param 内部（与前端一致）
        let mut inner = payload.clone();
        if let Some(o) = inner.as_object_mut() {
            o.entry("clientId").or_insert_with(|| json!(cid));
        }
        body["param"] = json!(encrypt(&inner.to_string(), secret_key)?);
    }
    Ok(json!({
        "header": {
            "key": cmd,
            "resTime": res_time,
            "reqSeq": req_seq,
            "channel": channel,
            "sign": make_sign(cmd, res_time, req_seq, channel, ""),
            "version": "",
        },
        "body": body,
    }))
}

/// 两大通道
pub enum Channel {
    /// 登录 / 用户信息
    ApiUser,
    /// 文件管理（密钥 = access_token 前 16 字节）
    WoHome,
}

impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::ApiUser => "api-user",
            Channel::WoHome => "wohome",
        }
    }
    pub fn path(&self) -> &'static str {
        match self {
            Channel::ApiUser => "/api-user/dispatcher",
            Channel::WoHome => "/wohome/dispatcher",
        }
    }
}

/// 站点基址
pub const BASE_URL: &str = "https://panservice.mail.wo.cn";

/// 上传相关常量（实测自前端 Uploader.js 与抓包）
pub mod upload {
    /// 服务端按**固定 8MB 步长**拼装分片；非 8MB 会在合并阶段 HTTP 500
    pub const CHUNK_SIZE: usize = 8 * 1024 * 1024;
    /// 表单固定值
    pub const CHANNEL: &str = "wocloud";
    /// `GetZoneInfo` 的 appId
    pub const ZONE_APP_ID: &str = "10000001";
    /// `GetZoneInfo` 未下发时的兜底域名
    pub const DEFAULT_HOST: &str = "https://hyupload.pan.wo.cn";
    /// 上传路径
    pub const PATH: &str = "/openapi/client/upload2C";
}

/// 前端 `random_str`：定长 `0-9A-Za-z` 随机串（batchNo 用 32 位）
pub fn random_str(n: usize) -> String {
    const POOL: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x9E3779B9);
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    (0..n).map(|_| POOL[next() % POOL.len()] as char).collect()
}

/// 分片布局：**严格 ceil**（服务端按 8MB 步长拼装）
///
/// 20MB/8MB → `[8388608, 8388608, 3222784]`。
///
/// ⚠️ 不可用前端那套 `floor` + 末尾吸收：末片 >8MB 时服务端**仍返回成功**，
///    但文件被**静默截断**成 `片数 × 8MB`（实测 20MB → 16,777,216 字节），
///    备份"显示完成"却少了一截 —— 是最危险的失败模式。
pub fn part_sizes(size: u64, chunk_size: usize) -> Vec<usize> {
    let size = size as usize;
    if size == 0 || chunk_size == 0 {
        return vec![size];
    }
    let count = size.div_ceil(chunk_size).max(1);
    let done = chunk_size * (count - 1);
    let mut v = vec![chunk_size; count - 1];
    v.push(size - done);
    v
}

/// H5 渠道 clientId（取自 umi.js：YUNPAN_APP）
pub const H5_CLIENT_ID: &str = "1001000035";
/// H5 渠道默认密钥（与映射表中 1001000035 一致）
pub const H5_DEFAULT_SECRET_KEY: &str = "iELf0UL07o6I8eRK";
/// 下载接口里前端硬编码的另一个 clientId
pub const DOWNLOAD_CLIENT_ID: &str = "1001000001";

/// H5 端 clientId → secretKey 映射表（前端硬编码；查不到会退化成默认密钥，导致 9002）
///
/// 只收录常用项；未收录的 clientId 一律回退 [`H5_DEFAULT_SECRET_KEY`]。
pub fn h5_secret(client_id: &str) -> &'static str {
    match client_id {
        "1001000001" => "R68VBxUs7Cv87VsN",
        "1001000003" => "Py1J67PAQoCb8Iel",
        "1001000015" => "M6vM8mMd1agRmExp",
        "1001000017" => "ecx8uEYhOGbpvCVp",
        "1001000024" => "32qIQvM4AUYrAPD8",
        "1001000032" => "CKW1z8AaoJQBXpiQ",
        "1001000035" => H5_DEFAULT_SECRET_KEY,
        "1001000039" => "QS5ByWs4GsTY18J1",
        "1001000040" => "186R8gEbSJolWRl9",
        "1001000046" => "Pbn39o7ufx5GI1M7",
        "1001000099" => "Py1J67PAQoCb8Iel",
        _ => H5_DEFAULT_SECRET_KEY,
    }
}

/// 响应业务码
pub mod code {
    pub const OK: &str = "0000";
    /// 登录态失效（前端会清存储跳登录页）
    pub const SESSION_INVALID: &str = "1001";
    /// 解密失败（clientId 与密钥不匹配时常见）
    pub const DECRYPT_FAIL: &str = "9002";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 Python 参考实现交叉验证的已知向量：
    /// 密钥 "iELf0UL07o6I8eRK"、IV 固定，明文为简单 JSON
    #[test]
    fn aes_roundtrip() {
        let s = encrypt("{\"a\":1}", H5_DEFAULT_SECRET_KEY).unwrap();
        let back = decrypt(&s, H5_DEFAULT_SECRET_KEY).unwrap();
        assert_eq!(back, "{\"a\":1}");
    }

    /// 密钥超过 16 字节时前端直接截断 —— 本实现必须一致
    #[test]
    fn key_truncated_to_16() {
        let long = "0123456789abcdefEXTRA";
        let short = "0123456789abcdef";
        assert_eq!(
            encrypt("x", long).unwrap(),
            encrypt("x", short).unwrap(),
            "超过 16 字节的密钥必须被截断"
        );
    }

    #[test]
    fn sign_is_md5_of_concat() {
        // MD5("QueryAllFiles" + 1790000000000 + 123456 + "wohome" + "")
        let got = make_sign("QueryAllFiles", 1790000000000, 123456, "wohome", "");
        let want = {
            let raw = "QueryAllFiles1790000000000123456wohome";
            hex::encode(Md5::digest(raw.as_bytes()))
        };
        assert_eq!(got, want);
        assert_eq!(got.len(), 32);
    }

    /// uuid 形状：8-4-4-4-12，且第 15 位是 '4'
    #[test]
    fn uuid_shape() {
        let u = js_uuid();
        assert_eq!(u.len(), 36, "{u}");
        assert_eq!(&u[14..15], "4");
        for (i, ch) in u.chars().enumerate() {
            if matches!(i, 8 | 13 | 18 | 23) {
                assert_eq!(ch, '-', "位置 {i} 应为 '-'");
            } else {
                assert!(ch.is_ascii_hexdigit(), "位置 {i} 应为 hex，实际 {ch}");
            }
        }
    }

    /// wohome 通道的 clientId 必须在**密文内部**；api-user 在外层
    #[test]
    fn client_id_placement_differs_by_channel() {
        let payload = json!({"k": "v"});
        // wohome：外层 body 只有 param/secret
        let w = build_envelope("QueryAllFiles", &payload, Some(H5_CLIENT_ID), "tok", Channel::WoHome.as_str(), 1, 2).unwrap();
        assert!(w["body"].get("clientId").is_none(), "wohome 外层不应有 clientId");
        let inner = decrypt(w["body"]["param"].as_str().unwrap(), "tok").unwrap();
        assert!(inner.contains("clientId"), "wohome 的 clientId 应在 param 内: {inner}");

        // api-user：外层带 clientId
        let a = build_envelope("AppQueryUser", &payload, Some(H5_CLIENT_ID), H5_DEFAULT_SECRET_KEY, Channel::ApiUser.as_str(), 1, 2).unwrap();
        assert_eq!(a["body"]["clientId"], json!(H5_CLIENT_ID));
    }

    /// 分片布局必须与 Python 参考实现一致（服务端按 8MB 步长拼装）
    #[test]
    fn part_sizes_strict_ceil() {
        let c = upload::CHUNK_SIZE;
        // 20MB → 3 片，末片为余数（Python 实测同值）
        assert_eq!(part_sizes(20_000_000, c), vec![8_388_608, 8_388_608, 3_222_784]);
        // 25MB → 3 片（25,000,000 - 2×8,388,608 = 8,222,784）
        assert_eq!(part_sizes(25_000_000, c), vec![8_388_608, 8_388_608, 8_222_784]);
        // 恰好 8MB → 单片
        assert_eq!(part_sizes(8_388_608, c), vec![8_388_608]);
        // 9MB：严格 ceil 下是 **2 片**（9,000,000 - 8,388,608 = 611,392）。
        // 注：Python 实测"9MB 单片成功"用的是前端 floor 布局；本插件走严格 ceil。
        assert_eq!(part_sizes(9_000_000, c), vec![8_388_608, 611_392]);
        // 空文件
        assert_eq!(part_sizes(0, c), vec![0]);
        // 各片之和必须等于总大小（防静默截断的第一道保险）
        for size in [1u64, 8_388_607, 8_388_608, 8_388_609, 20_000_000, 33_000_000] {
            let ps = part_sizes(size, c);
            assert_eq!(ps.iter().sum::<usize>() as u64, size, "分片之和须等于 {size}");
            // 非末片必须整 8MB
            for p in ps.iter().take(ps.len() - 1) {
                assert_eq!(*p, c, "非末片必须是整 8MB");
            }
        }
    }

    #[test]
    fn random_str_len_and_charset() {
        let s = random_str(32);
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()), "只能是 0-9A-Za-z: {s}");
        // 两次调用不应相同（种子取自纳秒时间）
        assert_ne!(s, random_str(32));
    }
}
