use crate::library::std::ai::memory::{VectorDatabase, VectorDbError};
use async_trait::async_trait;


use qdrant_client::prelude::*;
use qdrant_client::qdrant::point_id::PointIdOptions;
use qdrant_client::qdrant::vectors_config::Config;
use qdrant_client::qdrant::{
    CreateCollection, PointId, SearchPoints, SearchResponse, VectorParams, Vectors,
};



#[async_trait]
pub trait WrappedQdrantClient {
    async fn upsert_points_blocking(
        &self,
        collection_name: String,
        points: Vec<PointStruct>,
        option: Option<bool>,
    ) -> Result<(), String>;

    async fn search_points(&self, params: &SearchPoints) -> Result<SearchResponse, String>;

    async fn create_collection(&self, collection: &CreateCollection) -> Result<(), String>;
}

pub struct MyQdrantClient(QdrantClient);

#[async_trait]
impl WrappedQdrantClient for MyQdrantClient {
    async fn upsert_points_blocking(
        &self,
        _collection_name: String,
        _points: Vec<PointStruct>,
        _option: Option<bool>,
    ) -> Result<(), String> {
        // Actual implementation for QdrantClient
        Ok(())
    }

    async fn search_points(&self, _params: &SearchPoints) -> Result<SearchResponse, String> {
        // Actual implementation for QdrantClient
        Err("Not implemented".to_string())
    }

    async fn create_collection(&self, _collection: &CreateCollection) -> Result<(), String> {
        // Actual implementation for QdrantClient
        Err("Not implemented".to_string())
    }
}

struct MemoryQdrant<C: WrappedQdrantClient> {
    client: C,
}

#[async_trait]
impl<C: WrappedQdrantClient + Send + Sync> VectorDatabase<C> for MemoryQdrant<C> {
    fn attach_client(client: C) -> Result<Self, VectorDbError> {
        // Assuming QdrantClient can be constructed from a connection string
        // let client = MyQdrantClient(
        //     QdrantClient::new(Some(QdrantClientConfig {
        //         uri: connection_string.to_string(),
        //         ..Default::default()
        //     }))
        //     .unwrap(),
        // );
        Ok(MemoryQdrant { client })
    }

    async fn create_collection(
        &mut self,
        collection_name: String,
        embedding_length: u64,
    ) -> Result<(), VectorDbError> {
        self.client
            .create_collection(&CreateCollection {
                collection_name: collection_name.into(),
                vectors_config: Some(qdrant_client::qdrant::VectorsConfig {
                    config: Some(Config::Params(VectorParams {
                        size: embedding_length,
                        distance: Distance::Cosine.into(),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            })
            .await
            .map_err(|e| VectorDbError::CollectionCreationError(e.to_string()))
    }

    async fn insert_vector(
        &mut self,
        collection_name: String,
        id: u64,
        vector: Vec<f32>,
        payload: Option<chidori_prompt_format::serde_json::Value>,
    ) -> Result<(), VectorDbError> {
        // Additional payload handling here if needed
        let points = vec![PointStruct::new(
            PointId::from(id),
            Vectors::from(vector.to_vec()),
            if let Some(payload) = payload {
                //TODO: payload.try_into().unwrap()
                Payload::default()
            } else {
                Payload::default()
            },
        )];
        self.client
            .upsert_points_blocking(collection_name.clone(), points, None)
            .await
            .map_err(|e| VectorDbError::InsertionError(e.to_string())) // Map the error to VectorDbError
    }

    async fn query_by_vector(
        &mut self,
        collection_name: String,
        vector: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<u64>, VectorDbError> {
        let search_result = self
            .client
            .search_points(&SearchPoints {
                collection_name: collection_name.clone(),
                vector: vector.to_vec(),
                filter: None,
                limit: top_k as u64,
                with_payload: Some(true.into()),
                ..Default::default()
            })
            .await
            .map_err(|e| VectorDbError::QueryError(e.to_string()))?; // Map the error to VectorDbError

        let ids = search_result
            .result
            .into_iter()
            .filter_map(|point| {
                point.id.map(|id| match id.point_id_options.unwrap() {
                    PointIdOptions::Num(id) => id,
                    _ => 0,
                })
            })
            .collect();

        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrant_client::qdrant::PointId;
    use qdrant_client::qdrant::ScoredPoint;
    use qdrant_client::qdrant::Vectors;

    // Mock a QdrantClient for testing purposes
    struct MockQdrantClient {}

    impl MockQdrantClient {
        fn new(_connection_string: &str) -> Self {
            MockQdrantClient {}
        }
    }

    #[async_trait]
    impl WrappedQdrantClient for MockQdrantClient {
        async fn upsert_points_blocking(
            &self,
            _collection_name: String,
            _points: Vec<PointStruct>,
            _option: Option<bool>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn search_points(&self, _params: &SearchPoints) -> Result<SearchResponse, String> {
            Ok(SearchResponse {
                result: vec![
                    ScoredPoint {
                        id: Some(PointId::from(1)),
                        payload: Default::default(),
                        score: 0.9, // Example score
                        version: 1, // Example version
                        vectors: Some(Vectors::from(vec![0.1, 0.2])),
                    },
                    ScoredPoint {
                        id: Some(PointId::from(2)),
                        payload: Default::default(),
                        score: 0.8, // Example score
                        version: 1, // Example version
                        vectors: Some(Vectors::from(vec![0.3, 0.4])),
                    },
                ],
                time: 0.0,
            })
        }

        async fn create_collection(&self, _collection: &CreateCollection) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_insert_vector() {
        let mut db = MemoryQdrant {
            client: MockQdrantClient::new("mock_connection_string"),
        };

        let result = db
            .insert_vector("default".to_string(), 123, vec![0.5, 0.6], None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_query_by_vector() {
        let mut db = MemoryQdrant {
            client: MockQdrantClient::new("mock_connection_string"),
        };

        let result = db
            .query_by_vector("default".to_string(), vec![0.5, 0.6], 2)
            .await;
        assert!(result.is_ok());
        let ids = result.unwrap();
        assert_eq!(ids, vec![1, 2]);
    }
}
