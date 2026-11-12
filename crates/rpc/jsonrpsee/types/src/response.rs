// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Types pertaining to JSON-RPC responses.

use std::{borrow::Cow as StdCow, fmt};

use crate::{
    error::ErrorCode,
    params::{Id, SubscriptionId, TwoPointZero},
    request::Notification,
    ErrorObject, ErrorObjectOwned,
};
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};

/// JSON-RPC response object as defined in the [spec](https://www.jsonrpc.org/specification#response_object).
pub struct Response<'a, T: Clone> {
    /// JSON-RPC version.
    pub jsonrpc: Option<TwoPointZero>,
    /// Payload which can be result or error.
    pub payload: ResponsePayload<'a, T>,
    /// Request ID
    pub id: Id<'a>,
}

impl<'a, T: Clone> Response<'a, T> {
    /// Create a new [`Response`].
    pub fn new(payload: ResponsePayload<'a, T>, id: Id<'a>) -> Response<'a, T> {
        Response { jsonrpc: Some(TwoPointZero), payload, id }
    }

    /// Create an owned [`Response`].
    pub fn into_owned(self) -> Response<'static, T> {
        Response {
            jsonrpc: self.jsonrpc,
            payload: self.payload.into_owned(),
            id: self.id.into_owned(),
        }
    }
}

impl<T> fmt::Display for Response<'_, T>
where
    T: Serialize + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(&self).expect("valid JSON; qed"))
    }
}

impl<T> fmt::Debug for Response<'_, T>
where
    T: Serialize + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(&self).expect("valid JSON; qed"))
    }
}

/// Return value for subscriptions.
#[derive(Serialize, Deserialize, Debug)]
pub struct SubscriptionPayload<'a, T> {
    /// Subscription ID
    #[serde(borrow)]
    pub subscription: SubscriptionId<'a>,
    /// Result.
    pub result: T,
}

/// Subscription response object, embedding a [`SubscriptionPayload`] in the `params` member along
/// with `result` field.
pub type SubscriptionResponse<'a, T> = Notification<'a, SubscriptionPayload<'a, T>>;
/// Subscription response object, embedding a [`SubscriptionPayload`] in the `params` member along
/// with `error` field.
pub type SubscriptionError<'a, T> = Notification<'a, SubscriptionPayloadError<'a, T>>;

/// Error value for subscriptions.
#[derive(Serialize, Deserialize, Debug)]
pub struct SubscriptionPayloadError<'a, T> {
    /// Subscription ID
    #[serde(borrow)]
    pub subscription: SubscriptionId<'a>,
    /// Result.
    pub error: T,
}

/// Represent the payload of the JSON-RPC response object
///
/// It can be:
///
/// ```json
/// "result":<value>
/// "error":{"code":<code>,"message":<msg>,"data":<data>}
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ResponsePayload<'a, T>
where
    T: Clone,
{
    /// Corresponds to successful JSON-RPC response with the field `result`.
    Success(StdCow<'a, T>),
    /// Corresponds to failed JSON-RPC response with a error object with the field `error.
    Error(ErrorObject<'a>),
}

impl<'a, T: Clone> ResponsePayload<'a, T> {
    /// Create a successful owned response payload.
    pub fn success(t: T) -> Self {
        Self::Success(StdCow::Owned(t))
    }

    /// Create a successful borrowed response payload.
    pub fn success_borrowed(t: &'a T) -> Self {
        Self::Success(StdCow::Borrowed(t))
    }

    /// Convert the response payload into owned.
    pub fn into_owned(self) -> ResponsePayload<'static, T> {
        match self {
            Self::Error(e) => ResponsePayload::Error(e.into_owned()),
            Self::Success(r) => ResponsePayload::Success(StdCow::Owned(r.into_owned())),
        }
    }

    /// Create an error response payload.
    pub fn error(e: impl Into<ErrorObjectOwned>) -> Self {
        Self::Error(e.into())
    }

    /// Create a borrowed error response payload.
    pub fn error_borrowed(e: impl Into<ErrorObject<'a>>) -> Self {
        Self::Error(e.into())
    }
}

impl<'a, T: Clone> From<ErrorCode> for ResponsePayload<'a, T> {
    fn from(code: ErrorCode) -> ResponsePayload<'a, T> {
        Self::Error(code.into())
    }
}

impl<T> Serialize for Response<'_, T>
where
    T: Serialize + Clone,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("Response", 3)?;

        if let Some(field) = &self.jsonrpc {
            s.serialize_field("jsonrpc", field)?;
        }

        s.serialize_field("id", &self.id)?;

        match &self.payload {
            ResponsePayload::Error(err) => s.serialize_field("error", err)?,
            ResponsePayload::Success(r) => s.serialize_field("result", r)?,
        };

        s.end()
    }
}

#[cfg(test)]
mod tests {

    use super::{Id, Response, TwoPointZero};
    use crate::{response::ResponsePayload, ErrorObjectOwned};

    #[test]
    fn serialize_call_ok_response() {
        let ser = serde_json::to_string(&Response {
            jsonrpc: Some(TwoPointZero),
            payload: ResponsePayload::success("ok"),
            id: Id::Number(1),
        })
        .unwrap();
        let exp = r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#;
        assert_eq!(ser, exp);
    }

    #[test]
    fn serialize_call_err_response() {
        let ser = serde_json::to_string(&Response {
            jsonrpc: Some(TwoPointZero),
            payload: ResponsePayload::<()>::error(ErrorObjectOwned::owned(1, "lo", None::<()>)),
            id: Id::Number(1),
        })
        .unwrap();
        let exp = r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"lo"}}"#;
        assert_eq!(ser, exp);
    }

    #[test]
    fn serialize_call_response_missing_version_field() {
        let ser = serde_json::to_string(&Response {
            jsonrpc: None,
            payload: ResponsePayload::success("ok"),
            id: Id::Number(1),
        })
        .unwrap();
        let exp = r#"{"id":1,"result":"ok"}"#;
        assert_eq!(ser, exp);
    }
}
