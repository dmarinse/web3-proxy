use crate::{
    block_number::{BlockNumOrHash, CacheMode},
    errors::{Web3ProxyError, Web3ProxyResult},
    frontend::authorization::RequestOrMethod,
    jsonrpc::{self, JsonRpcErrorData, ResponsePayload},
};
use derive_more::From;
use ethers::{
    providers::{HttpClientError, JsonRpcError, ProviderError, WsClientError},
    types::U64,
};
use hashbrown::hash_map::DefaultHashBuilder;
use moka::future::Cache;
use serde_json::value::{to_raw_value, RawValue};
use std::{
    hash::{BuildHasher, Hash, Hasher},
    sync::Arc,
};

#[derive(Clone, Debug, Eq, From)]
pub struct JsonRpcQueryCacheKey<'a> {
    /// hashed params and block info so that we don't have to clone a potentially big thing
    /// this is probably a premature optimization
    hash: u64,
    from_block: Option<&'a BlockNumOrHash>,
    to_block: Option<&'a BlockNumOrHash>,
    cache_jsonrpc_errors: bool,
}

impl JsonRpcQueryCacheKey<'_> {
    #[inline]
    pub fn hash(&self) -> u64 {
        self.hash
    }
    #[inline]
    pub fn from_block_num(&self) -> Option<U64> {
        self.from_block.map(|x| x.num())
    }
    #[inline]
    pub fn to_block_num(&self) -> Option<U64> {
        self.to_block.map(|x| x.num())
    }
    #[inline]
    pub fn cache_errors(&self) -> bool {
        self.cache_jsonrpc_errors
    }
}

impl PartialEq for JsonRpcQueryCacheKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.hash.eq(&other.hash)
    }
}

impl Hash for JsonRpcQueryCacheKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // TODO: i feel like this hashes twice. oh well
        self.hash.hash(state);
    }
}

impl<'a> JsonRpcQueryCacheKey<'a> {
    pub fn new(cache_mode: &'a CacheMode, request: &'a RequestOrMethod) -> Self {
        // TODO: do this without clone
        let from_block = cache_mode.from_block();
        let to_block = cache_mode.to_block();
        let cache_block = cache_mode.cache_block();
        let cache_jsonrpc_errors = cache_mode.cache_jsonrpc_errors();

        let mut hasher = DefaultHashBuilder::default().build_hasher();

        from_block.hash(&mut hasher);
        to_block.hash(&mut hasher);
        cache_block.hash(&mut hasher);

        request.method().hash(&mut hasher);

        // TODO: make sure preserve_order feature is OFF
        // TODO: is there a faster way to do this? can we serialize directly into the hasher?
        request.params().to_string().hash(&mut hasher);

        cache_jsonrpc_errors.hash(&mut hasher);

        let hash = hasher.finish();

        Self {
            hash,
            from_block,
            to_block,
            cache_jsonrpc_errors,
        }
    }
}

pub type JsonRpcResponseCache = Cache<u64, ForwardedResponse<Arc<RawValue>>>;

/// TODO: think about this more. there is a lot of overlap with ParsedResponse
#[derive(Clone, Debug)]
pub enum ForwardedResponse<T> {
    Result {
        value: T,
        num_bytes: u64,
    },
    RpcError {
        error_data: JsonRpcErrorData,
        num_bytes: u64,
    },
}

// TODO: impl for other inner result types?
impl<R> ForwardedResponse<R> {
    #[inline]
    pub fn num_bytes(&self) -> u64 {
        match self {
            Self::Result { num_bytes, .. } => *num_bytes,
            Self::RpcError { num_bytes, .. } => *num_bytes,
        }
    }

    #[inline]
    pub fn is_error(&self) -> bool {
        match self {
            Self::Result { .. } => false,
            Self::RpcError { .. } => true,
        }
    }
}

impl<R> ForwardedResponse<Option<R>> {
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Result { value: None, .. })
    }
}

impl ForwardedResponse<Arc<RawValue>> {
    #[inline]
    pub fn is_null(&self) -> bool {
        match self {
            Self::Result { value, .. } => value.get() == "null",
            _ => false,
        }
    }
}

impl From<ResponsePayload<Arc<RawValue>>> for ForwardedResponse<Arc<RawValue>> {
    fn from(value: ResponsePayload<Arc<RawValue>>) -> Self {
        match value {
            ResponsePayload::Success { result } => {
                let num_bytes = result.get().len() as u64;

                ForwardedResponse::Result {
                    value: result,
                    num_bytes,
                }
            }
            ResponsePayload::Error { error } => {
                let num_bytes = error.num_bytes();

                ForwardedResponse::RpcError {
                    error_data: error,
                    num_bytes,
                }
            }
        }
    }
}

impl TryFrom<Web3ProxyResult<jsonrpc::SingleResponse>> for ForwardedResponse<Arc<RawValue>> {
    type Error = Web3ProxyError;
    fn try_from(response: Web3ProxyResult<jsonrpc::SingleResponse>) -> Result<Self, Self::Error> {
        match response? {
            jsonrpc::SingleResponse::Parsed(parsed) => match parsed.payload {
                jsonrpc::ResponsePayload::Success { result } => {
                    let num_bytes = result.get().len() as u64;

                    Ok(ForwardedResponse::Result {
                        value: result,
                        num_bytes,
                    })
                }
                jsonrpc::ResponsePayload::Error { error } => {
                    let num_bytes = error.num_bytes();

                    Ok(ForwardedResponse::RpcError {
                        error_data: error,
                        num_bytes,
                    })
                }
            },
            jsonrpc::SingleResponse::Stream(stream) => Err(Web3ProxyError::StreamResponse(stream)),
        }
    }
}

impl From<serde_json::Value> for ForwardedResponse<Arc<RawValue>> {
    fn from(value: serde_json::Value) -> Self {
        let value = to_raw_value(&value).unwrap();

        value.into()
    }
}

impl From<Arc<RawValue>> for ForwardedResponse<Arc<RawValue>> {
    fn from(value: Arc<RawValue>) -> Self {
        let num_bytes = value.get().len() as u64;

        Self::Result { value, num_bytes }
    }
}

impl From<Box<RawValue>> for ForwardedResponse<Arc<RawValue>> {
    fn from(value: Box<RawValue>) -> Self {
        let value: Arc<RawValue> = value.into();

        value.into()
    }
}

impl TryFrom<Web3ProxyError> for ForwardedResponse<Arc<RawValue>> {
    type Error = Web3ProxyError;

    fn try_from(value: Web3ProxyError) -> Result<Self, Self::Error> {
        match value {
            Web3ProxyError::EthersProvider(err) => match JsonRpcErrorData::try_from(&err) {
                Ok(x) => Ok(x.into()),
                Err(..) => Err(err.into()),
            },
            Web3ProxyError::JsonRpcErrorData(err) => Ok(err.into()),
            err => Err(err),
        }
    }
}

impl TryFrom<Result<Arc<RawValue>, Web3ProxyError>> for ForwardedResponse<Arc<RawValue>> {
    type Error = Web3ProxyError;

    fn try_from(value: Result<Arc<RawValue>, Web3ProxyError>) -> Result<Self, Self::Error> {
        match value {
            Ok(x) => Ok(x.into()),
            Err(err) => {
                let x: Self = err.try_into()?;

                Ok(x)
            }
        }
    }
}

impl<R> From<JsonRpcErrorData> for ForwardedResponse<R> {
    fn from(value: JsonRpcErrorData) -> Self {
        // TODO: wrap the error in a complete response?
        let num_bytes = serde_json::to_string(&value).unwrap().len() as u64;

        Self::RpcError {
            error_data: value,
            num_bytes,
        }
    }
}

impl<'a> From<&'a JsonRpcError> for JsonRpcErrorData {
    fn from(value: &'a JsonRpcError) -> Self {
        Self {
            code: value.code,
            message: value.message.clone().into(),
            data: value.data.clone(),
        }
    }
}

impl<'a> TryFrom<&'a ProviderError> for JsonRpcErrorData {
    type Error = &'a ProviderError;

    fn try_from(e: &'a ProviderError) -> Result<Self, Self::Error> {
        match e {
            ProviderError::JsonRpcClientError(err) => {
                match err.as_error_response() {
                    Some(err) => {
                        // this isn't safe to do because we don't have the id of the request
                        Ok(JsonRpcErrorData {
                            code: err.code,
                            message: err.message.clone().into(),
                            data: err.data.clone(),
                        })
                    }
                    None => Err(e),
                }
            }
            e => Err(e),
        }
    }
}

impl<'a> TryFrom<&'a HttpClientError> for JsonRpcErrorData {
    type Error = &'a HttpClientError;

    fn try_from(e: &'a HttpClientError) -> Result<Self, Self::Error> {
        match e {
            HttpClientError::JsonRpcError(err) => Ok(err.into()),
            e => Err(e),
        }
    }
}

impl<'a> TryFrom<&'a WsClientError> for JsonRpcErrorData {
    type Error = &'a WsClientError;

    fn try_from(e: &'a WsClientError) -> Result<Self, Self::Error> {
        match e {
            WsClientError::JsonRpcError(err) => Ok(err.into()),
            e => Err(e),
        }
    }
}

/// The inner u32 is the maximum weight per item
#[derive(Copy, Clone)]
pub struct JsonRpcResponseWeigher(pub u32);

impl JsonRpcResponseWeigher {
    pub fn weigh<K, T>(&self, _key: &K, value: &ForwardedResponse<T>) -> u32 {
        if let Ok(x) = value.num_bytes().try_into() {
            if x > self.0 {
                // return max. the item may start to be inserted into the cache, but it will be immediatly removed
                u32::MAX
            } else {
                x
            }
        } else {
            u32::MAX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ForwardedResponse;
    use crate::response_cache::JsonRpcResponseWeigher;
    use moka::future::{Cache, CacheBuilder};
    use serde_json::value::RawValue;
    use std::{sync::Arc, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn test_json_rpc_query_weigher() {
        let max_item_weight = 200;
        let weight_capacity = 1_000;

        let weigher = JsonRpcResponseWeigher(max_item_weight);

        let small_data: ForwardedResponse<Arc<RawValue>> = ForwardedResponse::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: (max_item_weight / 2) as u64,
        };

        assert_eq!(weigher.weigh(&(), &small_data), max_item_weight / 2);

        let max_sized_data: ForwardedResponse<Arc<RawValue>> = ForwardedResponse::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight as u64,
        };

        assert_eq!(weigher.weigh(&(), &max_sized_data), max_item_weight);

        let oversized_data: ForwardedResponse<Arc<RawValue>> = ForwardedResponse::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: (max_item_weight * 2) as u64,
        };

        assert_eq!(weigher.weigh(&(), &oversized_data), u32::MAX);

        let test_cache: Cache<u32, ForwardedResponse<Arc<RawValue>>> =
            CacheBuilder::new(weight_capacity)
                .weigher(move |k, v| weigher.weigh(k, v))
                .time_to_live(Duration::from_secs(2))
                .build();

        test_cache.insert(0, small_data).await;

        test_cache.get(&0).await.unwrap();

        test_cache.insert(1, max_sized_data).await;

        test_cache.get(&0).await.unwrap();
        test_cache.get(&1).await.unwrap();

        test_cache.insert(2, oversized_data).await;

        test_cache.get(&0).await.unwrap();
        test_cache.get(&1).await.unwrap();

        // oversized data will be in the cache temporarily (it should just be an arc though, so that should be fine)
        test_cache.get(&2).await.unwrap();

        // sync should do necessary cleanup
        test_cache.run_pending_tasks().await;

        // now it should be empty
        assert!(test_cache.get(&2).await.is_none());
    }
}
