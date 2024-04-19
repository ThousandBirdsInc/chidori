# Demonstrating RAG via stateful memory cells

Memory cells are an example of persistent stateful cells, once initially invoked they expose methods 
to the rest of the environment that manipulate their internal state. Memory specifically exposes "insert" and
"search" methods which are used to store and retrieve values respectively.

We must assign an embedding_fn to the memory cell, in this case we're using an embedding cell to simplify the process.
```memory (stateful_memory)
---
embedding_fn: rag
---
```

Embedding cells convert text into vector representations, most often this will be used
to convert text into a format that can be used by vector retrival, such as a memory cell.
The body of an embedding cell is a template, which you can use to compose multiple text inputs for
the purpose of embedding. You can also simply populate it with a single value, which is what we'll do here.
```embedding (rag)
---
model: facebook/rag-token-nq
---
{{message}}
```

We're going to read from the filesystem to get the text of the file, then we'll insert it into the memory cell.
In this case we'll be reading the contents of the file you're currently looking at, slicing it on newlines
and inserting each line into the memory cell.
```python
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
