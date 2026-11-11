//! S3 封装（对应文章 FR-6）：对象存储操作 + 预签名 URL。
//!
//! 路径约定层级化（如 `{project}/slides/{slide_id}/{image_id}.jpg`），
//! 前缀即「目录」，便于按前缀导出/删除。

use std::time::Duration;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::presigning::PresigningConfig;
use bytes::Bytes;

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("aws: {0}")]
    Aws(String),
}

/// 统一错误映射：S3 错误 → HTTP 语义错误。
impl From<S3Error> for operon_core::AppError {
    fn from(e: S3Error) -> Self {
        match e {
            S3Error::NotFound(m) => Self::NotFound(m),
            S3Error::Aws(m) => Self::Internal(m),
        }
    }
}

/// 单桶客户端。
#[derive(Clone)]
pub struct S3Client {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Client {
    pub fn new(aws_config: &aws_config::SdkConfig, bucket: impl Into<String>) -> Self {
        Self {
            client: aws_sdk_s3::Client::new(aws_config),
            bucket: bucket.into(),
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// 上传对象，返回 `s3://bucket/key`。
    pub async fn put_object(
        &self,
        key: &str,
        body: impl Into<ByteStream>,
        content_type: &str,
    ) -> Result<String, S3Error> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body.into())
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| S3Error::Aws(e.to_string()))?;
        Ok(format!("s3://{}/{}", self.bucket, key))
    }

    /// 读取对象。
    pub async fn get_object(&self, key: &str) -> Result<Bytes, S3Error> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.to_string().contains("NoSuchKey") || e.to_string().contains("404") {
                    S3Error::NotFound(key.into())
                } else {
                    S3Error::Aws(e.to_string())
                }
            })?;
        out.body
            .collect()
            .await
            .map(|d| d.into_bytes())
            .map_err(|e| S3Error::Aws(e.to_string()))
    }

    /// 预签名 GET（浏览器/客户端直接下载）。
    pub async fn presign_get(&self, key: &str, expires_secs: u64) -> Result<String, S3Error> {
        let conf = PresigningConfig::expires_in(Duration::from_secs(expires_secs))
            .map_err(|e| S3Error::Aws(e.to_string()))?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(conf)
            .await
            .map_err(|e| S3Error::Aws(e.to_string()))?;
        Ok(req.uri().to_string())
    }

    /// 预签名 PUT（浏览器/客户端直接上传）。
    pub async fn presign_put(&self, key: &str, expires_secs: u64) -> Result<String, S3Error> {
        let conf = PresigningConfig::expires_in(Duration::from_secs(expires_secs))
            .map_err(|e| S3Error::Aws(e.to_string()))?;
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(conf)
            .await
            .map_err(|e| S3Error::Aws(e.to_string()))?;
        Ok(req.uri().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_error_maps_to_app_error() {
        assert!(matches!(
            operon_core::AppError::from(S3Error::NotFound("x".into())),
            operon_core::AppError::NotFound(_)
        ));
        assert!(matches!(
            operon_core::AppError::from(S3Error::Aws("e".into())),
            operon_core::AppError::Internal(_)
        ));
    }

    #[tokio::test]
    async fn presign_generates_signed_url() {
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region("us-west-2")
            .load()
            .await;
        let s3 = S3Client::new(&cfg, "test-bucket");
        // presign 不实际发网络请求，只生成签名 URL
        let url = s3.presign_get("folder/file.txt", 300).await.unwrap();
        assert!(url.starts_with("https://test-bucket.s3"), "URL: {url}");
        assert!(url.contains("X-Amz-Signature"), "应有签名: {url}");

        let put_url = s3.presign_put("uploads/1.jpg", 600).await.unwrap();
        assert!(put_url.contains("X-Amz-Signature"));
    }
}
