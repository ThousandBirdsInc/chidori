# Chidori agent providing an interface and handling parsing EHR records into CPT Codes

```prompt
---
model: claude-3.5
fn: llm_extract_cpt
---
Parse the following EHR record {{record}} 
Extract CPT codes based on the contents of the record into a json format, 
include the cpt code and a description of the code and its meaning. Always include the description before the code name in the resulting json objects.
Return only the json payload itself, do not include additional explanation text.

The format for this json should be similar to:
[{description: "", code: ""}, {description: "", code: ""}] 
```

```python
import json

async def extract_cpt(record):
    encoded_result = json.loads(await llm_extract_cpt(record=record))
    return encoded_result
```

```javascript
import { Hono } from 'https://deno.land/x/hono/mod.ts';
import { serve } from 'https://deno.land/std@0.145.0/http/server.ts';

const app = new Hono();

app.get('/', (c) => {
  const form = `
    <!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>EHR Record Processor</title>
    <style>
        body {
            font-family: system-ui, -apple-system, sans-serif;
            max-width: 800px;
            margin: 2rem auto;
            padding: 0 1rem;
        }
        textarea {
            width: 100%;
            min-height: 200px;
            margin: 1rem 0;
            padding: 0.5rem;
            border: 1px solid #ccc;
            border-radius: 4px;
        }
        button {
            background-color: #0066cc;
            color: white;
            border: none;
            padding: 0.5rem 1rem;
            border-radius: 4px;
            cursor: pointer;
        }
        button:hover {
            background-color: #0052a3;
        }
        .results {
            margin-top: 2rem;
            padding: 1rem;
            border: 1px solid #ccc;
            border-radius: 4px;
            background-color: #f9f9f9;
        }
    </style>
</head>
<body>
    <h1>EHR Record Processor</h1>
    <form action="/submit" method="post">
        <div>
            <label for="ehrRecord">Enter EHR Record Text:</label>
            <textarea 
                id="ehrRecord" 
                name="ehrRecord" 
                placeholder="Paste the EHR record text here..."
                required
            ></textarea>
        </div>
        <button type="submit">Process Record</button>
    </form>
    <div id="results" class="results" style="display: none;">
        <h2>Processing Results</h2>
        <pre id="processedContent"></pre>
    </div>
</body>
</html>
  `;
  return c.html(form);
});

app.post('/submit', async (c) => {
    const body = await c.req.parseBody();
    const { ehrRecord } = body;

    // Call the extract_cpt function
    const cptResults = await extract_cpt(ehrRecord);

    // Create a formatted display of the CPT codes
    const response = `
    <h2>CPT Code Analysis Results</h2>
    <div class="results">
      <div class="raw-json">
        <h3>Raw JSON Data</h3>
        <pre>${JSON.stringify(cptResults, null, 2)}</pre>
      </div>
    </div>

    <style>
      .results {
        margin: 20px;
        padding: 20px;
        border-radius: 8px;
        background: #f8f9fa;
      }
      
      .cpt-grid {
        display: grid;
        gap: 1rem;
        margin: 1rem 0;
      }
      
      .cpt-item {
        padding: 1rem;
        background: white;
        border: 1px solid #dee2e6;
        border-radius: 4px;
        display: flex;
        flex-direction: column;
        gap: 0.5rem;
      }
      
      .cpt-code {
        color: #0066cc;
        font-size: 1.1em;
      }
      
      .cpt-description {
        color: #495057;
      }
      
      .raw-json {
        margin-top: 2rem;
        padding: 1rem;
        background: #f1f3f5;
        border-radius: 4px;
      }
      
      .raw-json pre {
        margin: 0;
        white-space: pre-wrap;
        word-wrap: break-word;
      }
    </style>
  `;

    return c.html(response);
});

serve(app.fetch);
```