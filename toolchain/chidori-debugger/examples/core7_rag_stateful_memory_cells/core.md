# Demonstrating RAG

We're going to read from the filesystem to get the text of the file, then we'll insert it into the memory cell.
In this case we'll be reading the contents of the file you're currently looking at, slicing it on newlines
and inserting each line into the memory cell.
```python

import os
from typing import List
import openai
from qdrant_client import QdrantClient
from qdrant_client.http import models

# Set up OpenAI API key
openai.api_key = os.getenv("OPENAI_API_KEY")

# Initialize Qdrant client
qdrant_client = QdrantClient("localhost", port=6333)

# Create a new collection for storing document embeddings
qdrant_client.recreate_collection(
    collection_name="documents",
    vectors_config=models.VectorParams(size=1536, distance=models.Distance.COSINE),
)

def get_embedding(text: str) -> List[float]:
    response = openai.Embedding.create(input=text, model="text-embedding-ada-002")
    return response["data"][0]["embedding"]

def add_document(text: str, metadata: dict):
    embedding = get_embedding(text)
    qdrant_client.upsert(
        collection_name="documents",
        points=[
            models.PointStruct(
                id=metadata["id"],
                vector=embedding,
                payload={"text": text, **metadata}
            )
        ]
    )

def search_documents(query: str, top_k: int = 3) -> List[dict]:
    query_embedding = get_embedding(query)
    search_result = qdrant_client.search(
        collection_name="documents",
        query_vector=query_embedding,
        limit=top_k
    )
    return [hit.payload for hit in search_result]

def generate_response(query: str, context: str) -> str:
    prompt = f"Context: {context}\n\nQuestion: {query}\n\nAnswer:"
    response = openai.Completion.create(
        engine="text-davinci-002",
        prompt=prompt,
        max_tokens=150,
        n=1,
        stop=None,
        temperature=0.7,
    )
    return response.choices[0].text.strip()

# Example usage
if __name__ == "__main__":
    # Add some sample documents
    add_document("The capital of France is Paris.", {"id": 1, "topic": "geography"})
    add_document("Python is a popular programming language.", {"id": 2, "topic": "programming"})
    add_document("The Eiffel Tower is located in Paris.", {"id": 3, "topic": "landmarks"})

    # Query the system
    query = "What is the capital of France?"
    relevant_docs = search_documents(query)
    context = " ".join([doc["text"] for doc in relevant_docs])
    response = generate_response(query, context)

    print(f"Query: {query}")
    print(f"Response: {response}")
    
def read_file_and_load_to_memory(file_path):
    with open(file_path, 'r') as file:
        content = file.read().split('\n')  # Splits the content into lines
        for line in content:
            insert(message=line)
    return content
```


To demonstrate this functionality we're going to search for lines that match the embedding "test".
```python (entry)
out = await read_file_and_load_to_memory("./")
```
