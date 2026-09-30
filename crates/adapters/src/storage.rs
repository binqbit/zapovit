use application::{BlobStore, Error, Result};
use async_trait::async_trait;
use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Credentials, Region, RequestChecksumCalculation,
        ResponseChecksumValidation,
    },
    primitives::ByteStream,
};
use std::time::Duration;

pub struct S3Storage {
    client: Client,
    bucket: String,
}
impl S3Storage {
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        key: &str,
        secret: &str,
    ) -> Result<Self> {
        let url = reqwest::Url::parse(endpoint).map_err(|_| Error::Config)?;
        if !["http", "https"].contains(&url.scheme())
            || url.host_str().is_none()
            || bucket.is_empty()
            || key.is_empty()
            || secret.is_empty()
        {
            return Err(Error::Config);
        }
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .endpoint_url(endpoint)
            .region(Region::new(region.to_owned()))
            .force_path_style(true)
            .credentials_provider(Credentials::new(
                key,
                secret,
                None,
                None,
                "explicit-local-garage",
            ))
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_timeout(Duration::from_secs(60))
                    .build(),
            )
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(2))
            .build();
        Ok(Self {
            client: Client::from_conf(config),
            bucket: bucket.into(),
        })
    }
}
#[async_trait]
impl BlobStore for S3Storage {
    async fn put(&self, key: &str, ciphertext: &[u8]) -> Result<()> {
        if ciphertext.len() > 15 * 1024 * 1024 {
            return Err(Error::InvalidInput);
        }
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/octet-stream")
            .body(ByteStream::from(ciphertext.to_vec()))
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }
    async fn get(&self, key: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        if output
            .content_length()
            .is_none_or(|n| n < 0 || n as u64 > max_bytes as u64)
        {
            return Err(Error::Storage);
        }
        let mut body = output.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|_| Error::Storage)?;
            if bytes.len() + chunk.len() > max_bytes {
                return Err(Error::Storage);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(false),
            Err(_) => Err(Error::Storage),
        }
    }
}
