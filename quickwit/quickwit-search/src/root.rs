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

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use futures::future::try_join_all;
use itertools::Itertools;
use quickwit_config::{build_doc_mapper, IndexConfig};
use quickwit_metastore::{Metastore, SplitMetadata};
use quickwit_proto::{
    FetchDocsRequest, FetchDocsResponse, Hit, LeafHit, LeafListTermsRequest, LeafListTermsResponse,
    LeafSearchRequest, LeafSearchResponse, ListTermsRequest, ListTermsResponse, PartialHit,
    SearchRequest, SearchResponse, SplitIdAndFooterOffsets,
};
use tantivy::aggregation::agg_result::AggregationResults;
use tantivy::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use tantivy::aggregation::AggregationLimits;
use tantivy::collector::Collector;
use tantivy::TantivyError;
use tracing::{debug, error, info_span, instrument};

use crate::cluster_client::ClusterClient;
use crate::collector::{make_merge_collector, QuickwitAggregations};
use crate::search_job_placer::Job;
use crate::service::SearcherContext;
use crate::{
    extract_split_and_footer_offsets, list_relevant_splits, SearchError, SearchJobPlacer,
    SearchServiceClient,
};

/// SearchJob to be assigned to search clients by the [`SearchJobPlacer`].
#[derive(Debug, PartialEq, Clone)]
pub struct SearchJob {
    cost: u32,
    offsets: SplitIdAndFooterOffsets,
}

impl SearchJob {
    #[cfg(test)]
    pub fn for_test(split_id: &str, cost: u32) -> SearchJob {
        SearchJob {
            cost,
            offsets: SplitIdAndFooterOffsets {
                split_id: split_id.to_string(),
                ..Default::default()
            },
        }
    }
}

impl From<SearchJob> for SplitIdAndFooterOffsets {
    fn from(search_job: SearchJob) -> Self {
        search_job.offsets
    }
}

impl<'a> From<&'a SplitMetadata> for SearchJob {
    fn from(split_metadata: &'a SplitMetadata) -> Self {
        SearchJob {
            cost: compute_split_cost(split_metadata),
            offsets: extract_split_and_footer_offsets(split_metadata),
        }
    }
}

impl Job for SearchJob {
    fn split_id(&self) -> &str {
        &self.offsets.split_id
    }

    fn cost(&self) -> u32 {
        self.cost
    }
}

pub(crate) struct FetchDocsJob {
    offsets: SplitIdAndFooterOffsets,
    pub partial_hits: Vec<PartialHit>,
}

impl Job for FetchDocsJob {
    fn split_id(&self) -> &str {
        &self.offsets.split_id
    }

    fn cost(&self) -> u32 {
        self.partial_hits.len() as u32
    }
}

impl From<FetchDocsJob> for SplitIdAndFooterOffsets {
    fn from(fetch_docs_job: FetchDocsJob) -> SplitIdAndFooterOffsets {
        fetch_docs_job.offsets
    }
}

pub(crate) fn validate_request(search_request: &SearchRequest) -> crate::Result<()> {
    if let Some(agg) = search_request.aggregation_request.as_ref() {
        let _aggs: QuickwitAggregations = serde_json::from_str(agg)
            .map_err(|err| SearchError::InvalidAggregationRequest(err.to_string()))?;
    };

    if search_request.start_offset > 10_000 {
        return Err(SearchError::InvalidArgument(format!(
            "max value for start_offset is 10_000, but got {}",
            search_request.start_offset
        )));
    }

    if search_request.max_hits > 10_000 {
        return Err(SearchError::InvalidArgument(format!(
            "max value for max_hits is 10_000, but got {}",
            search_request.max_hits
        )));
    }

    Ok(())
}

/// Performs a distributed search.
/// 1. Sends leaf request over gRPC to multiple leaf nodes.
/// 2. Merges the search results.
/// 3. Sends fetch docs requests to multiple leaf nodes.
/// 4. Builds the response with docs and returns.
#[instrument(skip(search_request, cluster_client, search_job_placer, metastore))]
pub async fn root_search(
    searcher_context: Arc<SearcherContext>,
    search_request: &SearchRequest,
    metastore: &dyn Metastore,
    cluster_client: &ClusterClient,
    search_job_placer: &SearchJobPlacer,
) -> crate::Result<SearchResponse> {
    let start_instant = tokio::time::Instant::now();

    let index_config: IndexConfig = metastore
        .index_metadata(&search_request.index_id)
        .await?
        .into_index_config();

    let doc_mapper = build_doc_mapper(&index_config.doc_mapping, &index_config.search_settings)
        .map_err(|err| {
            SearchError::InternalError(format!("Failed to build doc mapper. Cause: {err}"))
        })?;

    validate_request(search_request)?;

    // Validates the query by effectively building it against the current schema.
    doc_mapper.query(doc_mapper.schema(), search_request)?;

    let doc_mapper_str = serde_json::to_string(&doc_mapper).map_err(|err| {
        SearchError::InternalError(format!("Failed to serialize doc mapper: Cause {err}"))
    })?;

    let split_metadatas: Vec<SplitMetadata> =
        list_relevant_splits(search_request, metastore).await?;

    let split_offsets_map: HashMap<String, SplitIdAndFooterOffsets> = split_metadatas
        .iter()
        .map(|metadata| {
            (
                metadata.split_id().to_string(),
                extract_split_and_footer_offsets(metadata),
            )
        })
        .collect();

    let index_uri = &index_config.index_uri;

    let jobs: Vec<SearchJob> = split_metadatas.iter().map(SearchJob::from).collect();
    let assigned_leaf_search_jobs = search_job_placer.assign_jobs(jobs, &HashSet::default())?;
    debug!(assigned_leaf_search_jobs=?assigned_leaf_search_jobs, "Assigned leaf search jobs.");
    let leaf_search_responses: Vec<LeafSearchResponse> = try_join_all(
        assigned_leaf_search_jobs
            .into_iter()
            .map(|(client, client_jobs)| {
                let leaf_request = jobs_to_leaf_request(
                    search_request,
                    &doc_mapper_str,
                    index_uri.as_ref(),
                    client_jobs,
                );
                cluster_client.leaf_search(leaf_request, client)
            }),
    )
    .await?;

    // Creates a collector which merges responses into one
    let merge_collector = make_merge_collector(search_request, &searcher_context)?;
    let aggregations = merge_collector.aggregation.clone();

    // Merging is a cpu-bound task.
    // It should be executed by Tokio's blocking threads.

    // Wrap into result for merge_fruits
    let leaf_search_responses: Vec<tantivy::Result<LeafSearchResponse>> =
        leaf_search_responses.into_iter().map(Ok).collect_vec();
    let span = info_span!("merge_fruits");
    let leaf_search_response = crate::run_cpu_intensive(move || {
        let _span_guard = span.enter();
        merge_collector.merge_fruits(leaf_search_responses)
    })
    .await
    .context("failed to merge fruits")?
    .map_err(|merge_error: TantivyError| {
        crate::SearchError::InternalError(format!("{merge_error}"))
    })?;
    debug!(leaf_search_response = ?leaf_search_response, "Merged leaf search response.");

    if !leaf_search_response.failed_splits.is_empty() {
        error!(failed_splits = ?leaf_search_response.failed_splits, "Leaf search response contains at least one failed split.");
        let errors: String = leaf_search_response
            .failed_splits
            .iter()
            .map(|splits| format!("{splits}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(SearchError::InternalError(errors));
    }

    let client_fetch_docs_task: Vec<(SearchServiceClient, Vec<FetchDocsJob>)> =
        assign_client_fetch_doc_tasks(
            &leaf_search_response.partial_hits,
            &split_offsets_map,
            search_job_placer,
        )?;

    let fetch_docs_resp_futures =
        client_fetch_docs_task
            .into_iter()
            .map(|(client, fetch_docs_jobs)| {
                let partial_hits: Vec<PartialHit> = fetch_docs_jobs
                    .iter()
                    .flat_map(|fetch_doc_job| fetch_doc_job.partial_hits.iter().cloned())
                    .collect();
                let split_offsets: Vec<SplitIdAndFooterOffsets> = fetch_docs_jobs
                    .into_iter()
                    .map(|fetch_doc_job| fetch_doc_job.into())
                    .collect();

                let search_request_opt = if search_request.snippet_fields.is_empty() {
                    None
                } else {
                    Some(search_request.clone())
                };
                let fetch_docs_req = FetchDocsRequest {
                    partial_hits,
                    index_id: search_request.index_id.to_string(),
                    split_offsets,
                    index_uri: index_uri.to_string(),
                    search_request: search_request_opt,
                    doc_mapper: doc_mapper_str.clone(),
                };
                cluster_client.fetch_docs(fetch_docs_req, client)
            });

    let fetch_docs_resps: Vec<FetchDocsResponse> = try_join_all(fetch_docs_resp_futures).await?;

    // Merge the fetched docs.
    let leaf_hits = fetch_docs_resps
        .into_iter()
        .flat_map(|response| response.hits.into_iter());

    let mut hits: Vec<Hit> = leaf_hits
        .map(|leaf_hit: LeafHit| Hit {
            json: leaf_hit.leaf_json,
            partial_hit: leaf_hit.partial_hit,
            snippet: leaf_hit.leaf_snippet_json,
        })
        .collect();

    hits.sort_unstable_by_key(|hit| {
        Reverse(
            hit.partial_hit
                .as_ref()
                .map(|hit| hit.sorting_field_value)
                .unwrap_or(0),
        )
    });

    let elapsed = start_instant.elapsed();

    let aggregation = if let Some(intermediate_aggregation_result) =
        leaf_search_response.intermediate_aggregation_result
    {
        match aggregations.expect(
            "Aggregation should be present since we are processing an intermediate aggregation \
             result.",
        ) {
            QuickwitAggregations::FindTraceIdsAggregation(_) => {
                // The merge collector has already merged the intermediate results.
                Some(intermediate_aggregation_result)
            }
            QuickwitAggregations::TantivyAggregations(aggregations) => {
                let res: IntermediateAggregationResults =
                    serde_json::from_str(&intermediate_aggregation_result)?;
                let res: AggregationResults =
                    res.into_final_result(aggregations, &AggregationLimits::default())?;
                Some(serde_json::to_string(&res)?)
            }
        }
    } else {
        None
    };

    Ok(SearchResponse {
        aggregation,
        num_hits: leaf_search_response.num_hits,
        hits,
        elapsed_time_micros: elapsed.as_micros() as u64,
        errors: Vec::new(),
    })
}

/// Performs a distributed list terms.
/// 1. Sends leaf request over gRPC to multiple leaf nodes.
/// 2. Merges the search results.
/// 3. Builds the response and returns.
/// this is much simpler than `root_search` as it doesn't need to get actual docs.
#[instrument(skip(list_terms_request, cluster_client, search_job_placer, metastore))]
pub async fn root_list_terms(
    list_terms_request: &ListTermsRequest,
    metastore: &dyn Metastore,
    cluster_client: &ClusterClient,
    search_job_placer: &SearchJobPlacer,
) -> crate::Result<ListTermsResponse> {
    let start_instant = tokio::time::Instant::now();

    let index_config: IndexConfig = metastore
        .index_metadata(&list_terms_request.index_id)
        .await?
        .into_index_config();

    let doc_mapper = build_doc_mapper(&index_config.doc_mapping, &index_config.search_settings)
        .map_err(|err| {
            SearchError::InternalError(format!("Failed to build doc mapper. Cause: {err}"))
        })?;

    let schema = doc_mapper.schema();
    let field = schema.get_field(&list_terms_request.field).map_err(|_| {
        SearchError::InvalidQuery(format!(
            "Failed to list terms in `{}`, field doesn't exist",
            list_terms_request.field
        ))
    })?;

    let field_entry = schema.get_field_entry(field);
    if !field_entry.is_indexed() {
        return Err(SearchError::InvalidQuery(
            "Trying to list terms on field which isn't indexed".to_string(),
        ));
    }

    let mut query = quickwit_metastore::ListSplitsQuery::for_index(&list_terms_request.index_id)
        .with_split_state(quickwit_metastore::SplitState::Published);

    if let Some(start_ts) = list_terms_request.start_timestamp {
        query = query.with_time_range_start_gte(start_ts);
    }

    if let Some(end_ts) = list_terms_request.end_timestamp {
        query = query.with_time_range_end_lt(end_ts);
    }

    let split_metadatas = metastore
        .list_splits(query)
        .await?
        .into_iter()
        .map(|metadata| metadata.split_metadata)
        .collect::<Vec<_>>();

    let index_uri = &index_config.index_uri;

    let jobs: Vec<SearchJob> = split_metadatas.iter().map(SearchJob::from).collect();
    let assigned_leaf_search_jobs = search_job_placer.assign_jobs(jobs, &HashSet::default())?;
    debug!(assigned_leaf_search_jobs=?assigned_leaf_search_jobs, "Assigned leaf search jobs.");
    let leaf_search_responses: Vec<LeafListTermsResponse> = try_join_all(
        assigned_leaf_search_jobs
            .into_iter()
            .map(|(client, client_jobs)| {
                cluster_client.leaf_list_terms(
                    LeafListTermsRequest {
                        list_terms_request: Some(list_terms_request.clone()),
                        split_offsets: client_jobs.into_iter().map(|job| job.offsets).collect(),
                        index_uri: index_uri.to_string(),
                    },
                    client,
                )
            }),
    )
    .await?;

    let failed_splits: Vec<_> = leaf_search_responses
        .iter()
        .flat_map(|leaf_search_response| &leaf_search_response.failed_splits)
        .collect();

    if !failed_splits.is_empty() {
        error!(failed_splits = ?failed_splits, "Leaf search response contains at least one failed split.");
        let errors: String = failed_splits
            .iter()
            .map(|splits| splits.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(SearchError::InternalError(errors));
    }

    // Merging is a cpu-bound task, but probably fast enough to not require
    // spawning it on a blocking thread.

    let merged_iter = leaf_search_responses
        .into_iter()
        .map(|leaf_search_response| leaf_search_response.terms)
        .kmerge()
        .dedup();
    let leaf_list_terms_response: Vec<Vec<u8>> = if let Some(limit) = list_terms_request.max_hits {
        merged_iter.take(limit as usize).collect()
    } else {
        merged_iter.collect()
    };

    debug!(leaf_list_terms_response = ?leaf_list_terms_response, "Merged leaf search response.");

    let elapsed = start_instant.elapsed();

    Ok(ListTermsResponse {
        num_hits: leaf_list_terms_response.len() as u64,
        terms: leaf_list_terms_response,
        elapsed_time_micros: elapsed.as_micros() as u64,
        errors: Vec::new(),
    })
}

fn assign_client_fetch_doc_tasks(
    partial_hits: &[PartialHit],
    split_offsets_map: &HashMap<String, SplitIdAndFooterOffsets>,
    client_pool: &SearchJobPlacer,
) -> crate::Result<Vec<(SearchServiceClient, Vec<FetchDocsJob>)>> {
    // Group the partial hits per split
    let mut partial_hits_map: HashMap<String, Vec<PartialHit>> = HashMap::new();
    for partial_hit in partial_hits.iter() {
        partial_hits_map
            .entry(partial_hit.split_id.clone())
            .or_insert_with(Vec::new)
            .push(partial_hit.clone());
    }

    let mut fetch_docs_req_jobs: Vec<FetchDocsJob> = Vec::new();
    for (split_id, partial_hits) in partial_hits_map {
        let offsets = split_offsets_map
            .get(&split_id)
            .ok_or_else(|| {
                crate::SearchError::InternalError(format!(
                    "Received partial hit from an Unknown split {split_id}"
                ))
            })?
            .clone();
        let fetch_docs_job = FetchDocsJob {
            offsets,
            partial_hits,
        };
        fetch_docs_req_jobs.push(fetch_docs_job);
    }

    let assigned_jobs: Vec<(SearchServiceClient, Vec<FetchDocsJob>)> =
        client_pool.assign_jobs(fetch_docs_req_jobs, &HashSet::new())?;
    Ok(assigned_jobs)
}

// Measure the cost associated to searching in a given split metadata.
fn compute_split_cost(_split_metadata: &SplitMetadata) -> u32 {
    // TODO: Have a smarter cost, by smoothing the number of docs.
    1
}

/// Builds a [`LeafSearchRequest`] from a list of [`SearchJob`].
pub fn jobs_to_leaf_request(
    request: &SearchRequest,
    doc_mapper_str: &str,
    index_uri: &str, // TODO make Uri
    jobs: Vec<SearchJob>,
) -> LeafSearchRequest {
    let mut request_with_offset_0 = request.clone();
    request_with_offset_0.start_offset = 0;
    request_with_offset_0.max_hits += request.start_offset;
    LeafSearchRequest {
        search_request: Some(request_with_offset_0),
        split_offsets: jobs.into_iter().map(|job| job.offsets).collect(),
        doc_mapper: doc_mapper_str.to_string(),
        index_uri: index_uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use quickwit_config::SearcherConfig;
    use quickwit_grpc_clients::service_client_pool::ServiceClientPool;
    use quickwit_indexing::mock_split;
    use quickwit_metastore::{IndexMetadata, MockMetastore};
    use quickwit_proto::SplitSearchError;

    use super::*;
    use crate::MockSearchService;

    fn mock_partial_hit(
        split_id: &str,
        sorting_field_value: u64,
        doc_id: u32,
    ) -> quickwit_proto::PartialHit {
        quickwit_proto::PartialHit {
            sorting_field_value,
            split_id: split_id.to_string(),
            segment_ord: 1,
            doc_id,
        }
    }

    fn get_doc_for_fetch_req(
        fetch_docs_req: quickwit_proto::FetchDocsRequest,
    ) -> Vec<quickwit_proto::LeafHit> {
        fetch_docs_req
            .partial_hits
            .into_iter()
            .map(|req| quickwit_proto::LeafHit {
                leaf_json: serde_json::to_string_pretty(&serde_json::json!({
                    "title": [req.doc_id.to_string()],
                    "body": ["test 1"],
                    "url": ["http://127.0.0.1/1"]
                }))
                .expect("Json serialization should not fail"),
                partial_hit: Some(req),
                leaf_snippet_json: None,
            })
            .collect()
    }

    #[tokio::test]
    async fn test_root_search_offset_out_of_bounds_1085() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 10,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1"), mock_split("split2")]));
        let mut mock_search_service2 = MockSearchService::new();
        mock_search_service2.expect_leaf_search().returning(
            |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 3,
                    partial_hits: vec![
                        mock_partial_hit("split1", 3, 1),
                        mock_partial_hit("split1", 2, 2),
                        mock_partial_hit("split1", 1, 3),
                    ],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service2.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1.expect_leaf_search().returning(
            |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 2,
                    partial_hits: vec![
                        mock_partial_hit("split2", 3, 1),
                        mock_partial_hit("split2", 1, 3),
                    ],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let client_pool = ServiceClientPool::for_clients_list(vec![
            SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            ),
            SearchServiceClient::from_service(
                Arc::new(mock_search_service2),
                ([127, 0, 0, 1], 1001).into(),
            ),
        ]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 5);
        assert_eq!(search_response.hits.len(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_single_split() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));
        let mut mock_search_service = MockSearchService::new();
        mock_search_service.expect_leaf_search().returning(
            |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 3,
                    partial_hits: vec![
                        mock_partial_hit("split1", 3, 1),
                        mock_partial_hit("split1", 2, 2),
                        mock_partial_hit("split1", 1, 3),
                    ],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let client_pool =
            ServiceClientPool::for_clients_list(vec![SearchServiceClient::from_service(
                Arc::new(mock_search_service),
                ([127, 0, 0, 1], 1000).into(),
            )]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 3);
        assert_eq!(search_response.hits.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_multiple_splits() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1"), mock_split("split2")]));
        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1.expect_leaf_search().returning(
            |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 2,
                    partial_hits: vec![
                        mock_partial_hit("split1", 3, 1),
                        mock_partial_hit("split1", 1, 3),
                    ],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let mut mock_search_service2 = MockSearchService::new();
        mock_search_service2.expect_leaf_search().returning(
            |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 1,
                    partial_hits: vec![mock_partial_hit("split2", 2, 2)],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service2.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let client_pool = ServiceClientPool::for_clients_list(vec![
            SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            ),
            SearchServiceClient::from_service(
                Arc::new(mock_search_service2),
                ([127, 0, 0, 1], 1001).into(),
            ),
        ]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 3);
        assert_eq!(search_response.hits.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_multiple_splits_retry_on_other_node() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1"), mock_split("split2")]));

        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1
            .expect_leaf_search()
            .times(2)
            .returning(|leaf_search_req: quickwit_proto::LeafSearchRequest| {
                let split_ids: Vec<&str> = leaf_search_req
                    .split_offsets
                    .iter()
                    .map(|metadata| metadata.split_id.as_str())
                    .collect();
                if split_ids == ["split1"] {
                    Ok(quickwit_proto::LeafSearchResponse {
                        num_hits: 2,
                        partial_hits: vec![
                            mock_partial_hit("split1", 3, 1),
                            mock_partial_hit("split1", 1, 3),
                        ],
                        failed_splits: Vec::new(),
                        num_attempted_splits: 1,
                        ..Default::default()
                    })
                } else if split_ids == ["split2"] {
                    // RETRY REQUEST!
                    Ok(quickwit_proto::LeafSearchResponse {
                        num_hits: 1,
                        partial_hits: vec![mock_partial_hit("split2", 2, 2)],
                        failed_splits: Vec::new(),
                        num_attempted_splits: 1,
                        ..Default::default()
                    })
                } else {
                    panic!("unexpected request in test {split_ids:?}");
                }
            });
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );

        let mut mock_search_service2 = MockSearchService::new();
        mock_search_service2
            .expect_leaf_search()
            .times(1)
            .returning(|_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    // requests from split 2 arrive here - simulate failure
                    num_hits: 0,
                    partial_hits: Vec::new(),
                    failed_splits: vec![SplitSearchError {
                        error: "mock_error".to_string(),
                        split_id: "split2".to_string(),
                        retryable_error: true,
                    }],
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            });
        mock_search_service2.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let client_pool = ServiceClientPool::for_clients_list(vec![
            SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            ),
            SearchServiceClient::from_service(
                Arc::new(mock_search_service2),
                ([127, 0, 0, 1], 1001).into(),
            ),
        ]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 3);
        assert_eq!(search_response.hits.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_multiple_splits_retry_on_all_nodes() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1"), mock_split("split2")]));
        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1
            .expect_leaf_search()
            .withf(|leaf_search_req| leaf_search_req.split_offsets[0].split_id == "split2")
            .return_once(|_| {
                println!("request from service1 split2?");
                // requests from split 2 arrive here - simulate failure.
                // a retry will be made on the second service.
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 0,
                    partial_hits: Vec::new(),
                    failed_splits: vec![SplitSearchError {
                        error: "mock_error".to_string(),
                        split_id: "split2".to_string(),
                        retryable_error: true,
                    }],
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            });
        mock_search_service1
            .expect_leaf_search()
            .withf(|leaf_search_req| leaf_search_req.split_offsets[0].split_id == "split1")
            .return_once(|_| {
                println!("request from service1 split1?");
                // RETRY REQUEST from split1
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 2,
                    partial_hits: vec![
                        mock_partial_hit("split1", 3, 1),
                        mock_partial_hit("split1", 1, 3),
                    ],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            });
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let mut mock_search_service2 = MockSearchService::new();
        mock_search_service2
            .expect_leaf_search()
            .withf(|leaf_search_req| leaf_search_req.split_offsets[0].split_id == "split2")
            .return_once(|_| {
                println!("request from service2 split2?");
                // retry for split 2 arrive here, simulate success.
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 1,
                    partial_hits: vec![mock_partial_hit("split2", 2, 2)],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            });
        mock_search_service2
            .expect_leaf_search()
            .withf(|leaf_search_req| leaf_search_req.split_offsets[0].split_id == "split1")
            .return_once(|_| {
                println!("request from service2 split1?");
                // requests from split 1 arrive here - simulate failure, then success.
                Ok(quickwit_proto::LeafSearchResponse {
                    // requests from split 2 arrive here - simulate failure
                    num_hits: 0,
                    partial_hits: Vec::new(),
                    failed_splits: vec![SplitSearchError {
                        error: "mock_error".to_string(),
                        split_id: "split1".to_string(),
                        retryable_error: true,
                    }],
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            });
        mock_search_service2.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let client_pool = ServiceClientPool::for_clients_list(vec![
            SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            ),
            SearchServiceClient::from_service(
                Arc::new(mock_search_service2),
                ([127, 0, 0, 1], 1001).into(),
            ),
        ]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 3);
        assert_eq!(search_response.hits.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_single_split_retry_single_node() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));
        let mut first_call = true;
        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1
            .expect_leaf_search()
            .times(2)
            .returning(move |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                // requests from split 2 arrive here - simulate failure, then success
                if first_call {
                    first_call = false;
                    Ok(quickwit_proto::LeafSearchResponse {
                        num_hits: 0,
                        partial_hits: Vec::new(),
                        failed_splits: vec![SplitSearchError {
                            error: "mock_error".to_string(),
                            split_id: "split1".to_string(),
                            retryable_error: true,
                        }],
                        num_attempted_splits: 1,
                        ..Default::default()
                    })
                } else {
                    Ok(quickwit_proto::LeafSearchResponse {
                        num_hits: 1,
                        partial_hits: vec![mock_partial_hit("split1", 2, 2)],
                        failed_splits: Vec::new(),
                        num_attempted_splits: 1,
                        ..Default::default()
                    })
                }
            });
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        let client_pool =
            ServiceClientPool::for_clients_list(vec![SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            )]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 1);
        assert_eq!(search_response.hits.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_single_split_retry_single_node_fails() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));

        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1
            .expect_leaf_search()
            .times(2)
            .returning(move |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 0,
                    partial_hits: Vec::new(),
                    failed_splits: vec![SplitSearchError {
                        error: "mock_error".to_string(),
                        split_id: "split1".to_string(),
                        retryable_error: true,
                    }],
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            });
        mock_search_service1.expect_fetch_docs().returning(
            |_fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Err(SearchError::InternalError("mockerr docs".to_string()))
            },
        );
        let client_pool =
            ServiceClientPool::for_clients_list(vec![SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            )]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await;
        assert!(search_response.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_one_splits_two_nodes_but_one_is_failing_for_split(
    ) -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));
        // Service1 - broken node.
        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1.expect_leaf_search().returning(
            move |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                // retry requests from split 1 arrive here
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 1,
                    partial_hits: vec![mock_partial_hit("split1", 2, 2)],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        // Service2 - working node.
        let mut mock_search_service2 = MockSearchService::new();
        mock_search_service2.expect_leaf_search().returning(
            move |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 0,
                    partial_hits: Vec::new(),
                    failed_splits: vec![SplitSearchError {
                        error: "mock_error".to_string(),
                        split_id: "split1".to_string(),
                        retryable_error: true,
                    }],
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service2.expect_fetch_docs().returning(
            |_fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Err(SearchError::InternalError("mockerr docs".to_string()))
            },
        );
        let client_pool = ServiceClientPool::for_clients_list(vec![
            SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            ),
            SearchServiceClient::from_service(
                Arc::new(mock_search_service2),
                ([127, 0, 0, 1], 1001).into(),
            ),
        ]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 1);
        assert_eq!(search_response.hits.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_one_splits_two_nodes_but_one_is_failing_completely(
    ) -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));

        // Service1 - working node.
        let mut mock_search_service1 = MockSearchService::new();
        mock_search_service1.expect_leaf_search().returning(
            move |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Ok(quickwit_proto::LeafSearchResponse {
                    num_hits: 1,
                    partial_hits: vec![mock_partial_hit("split1", 2, 2)],
                    failed_splits: Vec::new(),
                    num_attempted_splits: 1,
                    ..Default::default()
                })
            },
        );
        mock_search_service1.expect_fetch_docs().returning(
            |fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Ok(quickwit_proto::FetchDocsResponse {
                    hits: get_doc_for_fetch_req(fetch_docs_req),
                })
            },
        );
        // Service2 - broken node.
        let mut mock_search_service2 = MockSearchService::new();
        mock_search_service2.expect_leaf_search().returning(
            move |_leaf_search_req: quickwit_proto::LeafSearchRequest| {
                Err(SearchError::InternalError("mockerr search".to_string()))
            },
        );
        mock_search_service2.expect_fetch_docs().returning(
            |_fetch_docs_req: quickwit_proto::FetchDocsRequest| {
                Err(SearchError::InternalError("mockerr docs".to_string()))
            },
        );
        let client_pool = ServiceClientPool::for_clients_list(vec![
            SearchServiceClient::from_service(
                Arc::new(mock_search_service1),
                ([127, 0, 0, 1], 1000).into(),
            ),
            SearchServiceClient::from_service(
                Arc::new(mock_search_service2),
                ([127, 0, 0, 1], 1001).into(),
            ),
        ]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await?;
        assert_eq!(search_response.num_hits, 1);
        assert_eq!(search_response.hits.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_invalid_queries() -> anyhow::Result<()> {
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split")]));

        let client_pool =
            ServiceClientPool::for_clients_list(vec![SearchServiceClient::from_service(
                Arc::new(MockSearchService::new()),
                ([127, 0, 0, 1], 1000).into(),
            )]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());

        assert!(root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &quickwit_proto::SearchRequest {
                index_id: "test-index".to_string(),
                query: r#"invalid_field:"test""#.to_string(),
                search_fields: vec!["body".to_string()],
                start_timestamp: None,
                end_timestamp: None,
                max_hits: 10,
                start_offset: 0,
                ..Default::default()
            },
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await
        .is_err());

        assert!(root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &quickwit_proto::SearchRequest {
                index_id: "test-index".to_string(),
                query: "test".to_string(),
                search_fields: vec!["invalid_field".to_string()],
                start_timestamp: None,
                end_timestamp: None,
                max_hits: 10,
                start_offset: 0,
                ..Default::default()
            },
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await
        .is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_invalid_aggregation() -> anyhow::Result<()> {
        let agg_req = r#"
            {
                "expensive_colors": {
                    "termss": {
                        "field": "color",
                        "order": {
                            "price_stats.max": "desc"
                        }
                    },
                    "aggs": {
                        "price_stats" : {
                            "stats": {
                                "field": "price"
                            }
                        }
                    }
                }
            }"#;

        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 0,
            aggregation_request: Some(agg_req.to_string()),
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));
        let client_pool =
            ServiceClientPool::for_clients_list(vec![SearchServiceClient::from_service(
                Arc::new(MockSearchService::new()),
                ([127, 0, 0, 1], 1000).into(),
            )]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await;
        assert!(search_response.is_err());
        assert_eq!(
            search_response.unwrap_err().to_string(),
            "Invalid aggregation request: data did not match any variant of untagged enum \
             QuickwitAggregations",
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_root_search_invalid_request() -> anyhow::Result<()> {
        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 10,
            start_offset: 20_000,
            ..Default::default()
        };
        let mut metastore = MockMetastore::new();
        metastore
            .expect_index_metadata()
            .returning(|_index_id: &str| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_list_splits()
            .returning(|_filter| Ok(vec![mock_split("split1")]));
        let client_pool =
            ServiceClientPool::for_clients_list(vec![SearchServiceClient::from_service(
                Arc::new(MockSearchService::new()),
                ([127, 0, 0, 1], 1000).into(),
            )]);
        let search_job_placer = SearchJobPlacer::new(client_pool);
        let cluster_client = ClusterClient::new(search_job_placer.clone());
        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await;
        assert!(search_response.is_err());
        assert_eq!(
            search_response.unwrap_err().to_string(),
            "Invalid argument: max value for start_offset is 10_000, but got 20000",
        );

        let search_request = quickwit_proto::SearchRequest {
            index_id: "test-index".to_string(),
            query: "test".to_string(),
            search_fields: vec!["body".to_string()],
            start_timestamp: None,
            end_timestamp: None,
            max_hits: 20_000,
            ..Default::default()
        };

        let search_response = root_search(
            Arc::new(SearcherContext::new(SearcherConfig::default())),
            &search_request,
            &metastore,
            &cluster_client,
            &search_job_placer,
        )
        .await;
        assert!(search_response.is_err());
        assert_eq!(
            search_response.unwrap_err().to_string(),
            "Invalid argument: max value for max_hits is 10_000, but got 20000",
        );

        Ok(())
    }
}
