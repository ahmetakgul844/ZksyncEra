//! Tokens part of API implementation.

// Built-in uses

// External uses
use actix_web::{
    web::{self, Json},
    Scope,
};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

// Workspace uses
use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
};
use zksync_storage::{ConnectionPool, QueryResult};
use zksync_types::{Token, TokenLike};

// Local uses
use super::{
    client::{self, Client},
    Error as ApiError, JsonResult,
};
use crate::fee_ticker::{TickerRequest, TokenPriceRequestType};

/// Shared data between `api/v1/tokens` endpoints.
#[derive(Clone)]
struct ApiTokensData {
    pool: ConnectionPool,
    fee_ticker: mpsc::Sender<TickerRequest>,
    // TODO reimplement tokens cache.
}

impl ApiTokensData {
    fn new(pool: ConnectionPool, fee_ticker: mpsc::Sender<TickerRequest>) -> Self {
        Self { pool, fee_ticker }
    }

    async fn tokens(&self) -> QueryResult<Vec<Token>> {
        let mut storage = self.pool.access_storage().await?;

        let tokens = storage.tokens_schema().load_tokens().await?;
        Ok(tokens.into_iter().map(|(_k, v)| v).collect())
    }

    async fn token(&self, token_like: TokenLike) -> QueryResult<Option<Token>> {
        let mut storage = self.pool.access_storage().await?;

        storage.tokens_schema().get_token(token_like).await
    }

    async fn token_price_usd(&self, token: TokenLike) -> QueryResult<Option<BigDecimal>> {
        let (price_sender, price_receiver) = oneshot::channel();
        self.fee_ticker
            .clone()
            .send(TickerRequest::GetTokenPrice {
                token,
                response: price_sender,
                req_type: TokenPriceRequestType::USDForOneToken,
            })
            .await?;

        // Ugly hack to distinguish real error from missing token.
        match price_receiver.await? {
            Ok(price) => Ok(Some(price)),
            Err(err) => {
                // TODO: Improve ticker API to remove this terrible code snippet. (task number ????)
                if err.to_string().contains("Token not found") {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
        }
    }
}

// Data transfer objects.

#[derive(Debug, Deserialize, Serialize, Copy, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TokenPriceKind {
    Currency,
    Token,
}

#[derive(Debug, Deserialize, Serialize, Copy, Clone, PartialEq)]
struct TokenPriceQuery {
    #[serde(rename = "in")]
    kind: TokenPriceKind,
}

// Client implementation

/// Tokens API part.
impl Client {
    pub async fn tokens(&self) -> client::Result<Vec<Token>> {
        self.get("tokens").send().await
    }

    pub async fn token_by_id(&self, token: &TokenLike) -> client::Result<Option<Token>> {
        self.get(&format!("tokens/{}", token)).send().await
    }

    pub async fn token_price(
        &self,
        token: &TokenLike,
        kind: TokenPriceKind,
    ) -> client::Result<Option<BigDecimal>> {
        self.get(&format!("tokens/{}/price", token))
            .query(&TokenPriceQuery { kind })
            .send()
            .await
    }
}

// Server implementation

async fn tokens(data: web::Data<ApiTokensData>) -> JsonResult<Vec<Token>> {
    let tokens = data.tokens().await.map_err(ApiError::internal)?;

    Ok(Json(tokens))
}

async fn token_by_id(
    data: web::Data<ApiTokensData>,
    web::Path(token_like): web::Path<String>,
) -> JsonResult<Option<Token>> {
    let token_like = TokenLike::parse(&token_like);

    let token = data.token(token_like).await.map_err(ApiError::internal)?;
    Ok(Json(token))
}

async fn token_price(
    data: web::Data<ApiTokensData>,
    web::Path(token_like): web::Path<String>,
    web::Query(token_query): web::Query<TokenPriceQuery>,
) -> JsonResult<Option<BigDecimal>> {
    let token_like = TokenLike::parse(&token_like);

    let price = match token_query.kind {
        TokenPriceKind::Currency => data
            .token_price_usd(token_like)
            .await
            .map_err(ApiError::internal)?,

        TokenPriceKind::Token => {
            return Err(ApiError::not_implemented(
                "price in tokens not yet implemented",
            ))
        }
    };

    Ok(Json(price))
}

pub fn api_scope(pool: ConnectionPool, fee_ticker: mpsc::Sender<TickerRequest>) -> Scope {
    let data = ApiTokensData::new(pool, fee_ticker);

    web::scope("tokens")
        .data(data)
        .route("", web::get().to(tokens))
        .route("{id}", web::get().to(token_by_id))
        .route("{id}/price", web::get().to(token_price))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zksync_types::Address;

    use super::{super::test_utils::TestServerConfig, *};

    fn dummy_fee_ticker(prices: &[(TokenLike, BigDecimal)]) -> mpsc::Sender<TickerRequest> {
        let (sender, mut receiver) = mpsc::channel(10);

        let prices: HashMap<_, _> = prices.iter().cloned().collect();
        actix_rt::spawn(async move {
            while let Some(item) = receiver.next().await {
                match item {
                    TickerRequest::GetTokenPrice {
                        token,
                        response,
                        req_type,
                    } => {
                        assert_eq!(
                            req_type,
                            TokenPriceRequestType::USDForOneToken,
                            "Unsupported price request type"
                        );

                        let msg = if let Some(price) = prices.get(&token) {
                            Ok(price.clone())
                        } else {
                            // To provide compatibility with the `token_price_usd` hack.
                            Err(anyhow::format_err!("Token not found: {:?}", token))
                        };

                        response.send(msg).expect("Unable to send response");
                    }
                    _ => unreachable!("Unsupported request"),
                }
            }
        });

        sender
    }

    #[actix_rt::test]
    async fn test_tokens_scope() -> anyhow::Result<()> {
        let cfg = TestServerConfig::default();
        cfg.fill_database().await?;

        let prices = [
            (TokenLike::Id(1), 10_u64.into()),
            (TokenLike::Id(15), 10_500_u64.into()),
            ("ETH".into(), 0_u64.into()),
            (Address::default().into(), 1_u64.into()),
        ];
        let fee_ticker = dummy_fee_ticker(&prices);

        let (client, server) =
            cfg.start_server(move |cfg| api_scope(cfg.pool.clone(), fee_ticker.clone()));

        // Fee requests
        for (token, expected_price) in &prices {
            let actual_price = client.token_price(token, TokenPriceKind::Currency).await?;

            assert_eq!(
                actual_price.as_ref(),
                Some(expected_price),
                "Price does not match"
            );
        }
        assert_eq!(
            client
                .token_price(&TokenLike::Id(2), TokenPriceKind::Currency)
                .await?,
            None
        );
        // TODO Check error (#1152)
        client
            .token_price(&TokenLike::Id(2), TokenPriceKind::Token)
            .await
            .unwrap_err();

        // Tokens requests
        let expected_tokens = {
            let mut storage = cfg.pool.access_storage().await?;

            storage.tokens_schema().load_tokens().await?
        };

        assert_eq!(
            client.tokens().await?,
            expected_tokens.values().cloned().collect::<Vec<_>>()
        );

        let expected_token = expected_tokens.values().cloned().next();
        assert_eq!(client.token_by_id(&TokenLike::Id(0)).await?, expected_token);
        assert_eq!(
            client
                .token_by_id(&TokenLike::parse(
                    "0x0000000000000000000000000000000000000000"
                ))
                .await?,
            expected_token
        );
        assert_eq!(
            client
                .token_by_id(&TokenLike::parse(
                    "0000000000000000000000000000000000000000"
                ))
                .await?,
            expected_token
        );
        assert_eq!(
            client.token_by_id(&TokenLike::parse("ETH")).await?,
            expected_token
        );
        assert_eq!(client.token_by_id(&TokenLike::parse("XM")).await?, None);

        server.stop().await;
        Ok(())
    }
}
