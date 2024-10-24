# Demonstrates a contextually relevant demo

```prompt
---
model: claude-3.5
fn: generate_ehr
---
Create an example EHR record
```

```prompt
---
model: claude-3.5
fn: extract_cpt
---
Extract cpt codes from the provided EHR record {{record}}
```

```python
ehr = await generate_ehr()
```

```python
cpt = await extract_cpt(record=ehr)
```
