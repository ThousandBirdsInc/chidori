use anyhow;
use async_trait::async_trait;


pub mod in_memory;
pub mod qdrant;

// Define a custom error type for our vector database interactions

#[derive(Debug)]
pub enum VectorDbError {
    ConnectionError(String),
    QueryError(String),
    InsertionError(String),
    CollectionCreationError(String),
    // other error types...
}

struct CoreValueEmbedding {}

trait TraitValueEmbedding {
    fn embed(&self) -> anyhow::Result<Vec<f32>>;
}

struct CoreVectorDatabase {
    name: String,
    table: String,
    schema: String,
}

// The trait for vector database interaction
#[async_trait]
pub trait VectorDatabase<C> {
    // Connects to the vector database
    fn attach_client(client: C) -> Result<Self, VectorDbError>
    where
        Self: Sized;

    async fn create_collection(
        &mut self,
        collection_name: String,
        embedding_length: u64,
    ) -> Result<(), VectorDbError>;

    // Inserts a vector into the database
    async fn insert_vector(
        &mut self,
        collection_name: String,
        id: u64,
        vector: Vec<f32>,
        payload: Option<chidori_prompt_format::serde_json::Value>,
    ) -> Result<(), VectorDbError>;

    // Queries the database by vector
    async fn query_by_vector(
        &mut self,
        collection_name: String,
        vector: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<u64>, VectorDbError>;

    // Additional methods like update, delete, etc. can be added here
}
