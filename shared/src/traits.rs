use axum::{
    async_trait,
    extract::{self, FromRequest, FromRequestParts, Path, Query},
    http::StatusCode,
    BoxError, RequestPartsExt,
};
use fundu::DurationParser;
use http::request::Parts;

use crate::*;

#[derive(Deserialize)]
struct HowLongToBlockQueryExtractor {
    wait_time: Option<String>,
    wait_count: Option<u16>,
}

#[test]
fn test_duration_parsing() {
    let mut parser = DurationParser::default();
    let parser = parser.default_unit(fundu::TimeUnit::MilliSecond);
    assert_eq!(parser.parse("1234s").unwrap().as_millis(), 1234000);
    assert_eq!(parser.parse("1234").unwrap().as_millis(), 1234);
}

#[async_trait]
impl<S> FromRequestParts<S> for HowLongToBlock
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(req: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match req.extract::<Query<HowLongToBlockQueryExtractor>>().await {
            Ok(Query(HowLongToBlockQueryExtractor { wait_time, wait_count })) => {
                if let Some(wait_time_str) = wait_time {
                    let wait_time = DurationParser::default()
                        .default_unit(fundu::TimeUnit::MilliSecond)
                        .parse(&wait_time_str)
                        .map_err(|_| (StatusCode::BAD_REQUEST, "For long-polling, please define &wait_time=<duration with unit> (e.g. 1000ms) and &wait_count=<count>."))?;
                    Ok(Self { wait_time: Some(wait_time), wait_count})
                } else {
                    Ok(Self { wait_time: None, wait_count })
                }
            },
            Err(_) => Err((StatusCode::BAD_REQUEST, "For long-polling, please define &wait_time=<duration with unit> (e.g. 1000ms) and &wait_count=<count>.")),
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for MyUuid
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(req: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match req.extract::<Path<Uuid>>().await {
            Ok(value) => Ok(Self(value.0)),
            Err(_) => Err((StatusCode::BAD_REQUEST, "Invalid ID supplied.")),
        }
    }
}
