# Prompt Graph Code

This implements an interface for constructing prompt graphs.
This can be used to annotate existing implementations with graph definitions as well.

Lambda definitions should include an endpoint to return the serialized prompt graph.

## Generating Python Client
- maturin develop --features python

## Generating Nodejs Client
- npm run build -- --release