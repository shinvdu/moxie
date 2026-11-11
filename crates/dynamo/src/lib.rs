//! DynamoDB 薄封装。
//!
//! 对应文章 4.4 节：「薄包装，不做 ORM」。不做查询 DSL、不做关系映射、
//! 不做迁移工具，只让 80% 的日常读写操作写起来舒服：
//! - 自动 serde 序列化 / 反序列化（`serde_dynamo`）；
//! - 统一的 `DynamoError`，handler 层再映射成 HTTP 状态码。

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// DynamoDB 返回的原始 item 类型（serde_dynamo 需要它作为反序列化输入）。
type Item = HashMap<String, aws_sdk_dynamodb::types::AttributeValue>;

#[derive(Debug, thiserror::Error)]
pub enum DynamoError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conditional check failed")]
    ConditionalCheckFailed,
    #[error("aws: {0}")]
    Aws(String),
}

/// 统一错误映射：DynamoDB 错误 → HTTP 语义错误。
/// 对应文章 4.4 节「ResourceNotFoundException 自动变成 HTTP 404」。
impl From<DynamoError> for operon_core::AppError {
    fn from(e: DynamoError) -> Self {
        match e {
            DynamoError::NotFound(m) => Self::NotFound(m),
            DynamoError::ConditionalCheckFailed => {
                Self::Conflict("conditional check failed".into())
            }
            DynamoError::Aws(m) => Self::Internal(m),
        }
    }
}

/// 单表客户端：一个客户端绑定一张表（表名已带环境前缀）。
#[derive(Clone)]
pub struct DynamoClient {
    client: aws_sdk_dynamodb::Client,
    table: String,
}

impl DynamoClient {
    pub fn new(aws_config: &aws_config::SdkConfig, table: impl Into<String>) -> Self {
        Self {
            client: aws_sdk_dynamodb::Client::new(aws_config),
            table: table.into(),
        }
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// 单条读取：`pk` / `sk` 单表主键模型。
    pub async fn get<T: DeserializeOwned>(
        &self,
        pk: &str,
        sk: &str,
    ) -> Result<Option<T>, DynamoError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("pk", aws_sdk_dynamodb::types::AttributeValue::S(pk.into()))
            .key("sk", aws_sdk_dynamodb::types::AttributeValue::S(sk.into()))
            .send()
            .await
            .map_err(|e| DynamoError::Aws(e.to_string()))?;
        match out.item() {
            Some(item) => serde_dynamo::from_item::<Item, T>(item.clone())
                .map(Some)
                .map_err(|e| DynamoError::Aws(e.to_string())),
            None => Ok(None),
        }
    }

    /// 写入（整体覆盖）。
    pub async fn put<T: Serialize>(&self, item: &T) -> Result<(), DynamoError> {
        let item = serde_dynamo::to_item(item)
            .map_err(|e| DynamoError::Aws(e.to_string()))?;
        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .send()
            .await
            .map_err(|e| DynamoError::Aws(e.to_string()))?;
        Ok(())
    }

    pub async fn delete(&self, pk: &str, sk: &str) -> Result<(), DynamoError> {
        self.client
            .delete_item()
            .table_name(&self.table)
            .key("pk", aws_sdk_dynamodb::types::AttributeValue::S(pk.into()))
            .key("sk", aws_sdk_dynamodb::types::AttributeValue::S(sk.into()))
            .send()
            .await
            .map_err(|e| DynamoError::Aws(e.to_string()))?;
        Ok(())
    }

    /// 按主键 `pk` 查询所有同键记录（SK 升序）。
    pub async fn query<T: DeserializeOwned>(&self, pk: &str) -> Result<Vec<T>, DynamoError> {
        self.query_key_condition("pk", pk).await
    }

    /// 按 GSI 查询（key 为 GSI 分区键列，pk 为值）。
    pub async fn query_index<T: DeserializeOwned>(
        &self,
        index: &str,
        pk: &str,
    ) -> Result<Vec<T>, DynamoError> {
        self.query_key_condition_with_index(index, "gsi1pk", pk).await
    }

    async fn query_key_condition<T: DeserializeOwned>(
        &self,
        key_col: &str,
        pk: &str,
    ) -> Result<Vec<T>, DynamoError> {
        self.query_key_condition_with_index("", key_col, pk).await
    }

    async fn query_key_condition_with_index<T: DeserializeOwned>(
        &self,
        index: &str,
        key_col: &str,
        pk: &str,
    ) -> Result<Vec<T>, DynamoError> {
        let mut req = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression(format!("{key_col} = :pk"))
            .expression_attribute_values(
                ":pk",
                aws_sdk_dynamodb::types::AttributeValue::S(pk.into()),
            );
        if !index.is_empty() {
            req = req.index_name(index);
        }
        let out = req
            .send()
            .await
            .map_err(|e| DynamoError::Aws(e.to_string()))?;
        let mut items = Vec::new();
        for item in out.items() {
            let v: T = serde_dynamo::from_item::<Item, T>(item.clone())
                .map_err(|e| DynamoError::Aws(e.to_string()))?;
            items.push(v);
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use operon_core::AppError;

    #[test]
    fn dynamo_error_maps_to_app_error() {
        assert!(matches!(AppError::from(DynamoError::NotFound("x".into())), AppError::NotFound(_)));
        assert!(matches!(AppError::from(DynamoError::ConditionalCheckFailed), AppError::Conflict(_)));
        assert!(matches!(AppError::from(DynamoError::Aws("e".into())), AppError::Internal(_)));
    }
}
