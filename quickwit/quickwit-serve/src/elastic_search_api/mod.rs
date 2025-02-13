// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

mod api_specs;
mod rest_handler;

use std::sync::Arc;

pub(crate) use quickwit_common::simple_list::{from_simple_list, to_simple_list, SimpleList};
use quickwit_search::SearchService;
use serde::{Deserialize, Serialize};
use warp::{Filter, Rejection};

use crate::elastic_search_api::rest_handler::{
    es_compat_index_search_handler, es_compat_search_handler,
};

/// Setup Elasticsearch API handlers
///
/// This is where all newly supported Elasticsearch handlers
/// should be registered.
pub fn elastic_api_handlers(
    search_service: Arc<dyn SearchService>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    es_compat_search_handler(search_service.clone())
        .or(es_compat_index_search_handler(search_service.clone()))
    // Register newly created handlers here.
}

/// Helper type needed by the Elasticsearch endpoints.
/// Control how the total number of hits should be tracked.
///
/// When set to `Track` with a value `true`, the response will always track the number of hits that
/// match the query accurately.
///
/// When set to `Count` with an integer value `n`, the response accurately tracks the total
/// hit count that match the query up to `n` documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TrackTotalHits {
    /// Track the number of hits that match the query accurately.
    Track(bool),
    /// Track the number of hits up to the specified value.
    Count(i64),
}

impl From<bool> for TrackTotalHits {
    fn from(b: bool) -> Self {
        TrackTotalHits::Track(b)
    }
}

impl From<i64> for TrackTotalHits {
    fn from(i: i64) -> Self {
        TrackTotalHits::Count(i)
    }
}
