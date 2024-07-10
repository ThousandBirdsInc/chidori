# Demonstrating how to leverage instantiation of multiple execution graphs to simulate and monitor a multi-agent system

This example is based on [this crewAI example](https://github.com/joaomdmoura/crewAI-examples/tree/main/trip_planner)

```prompt (researcher)
---
model: gpt-3.5-turbo
fn: scrape_and_summarize_website_researcher
---
Role: Principal Researcher
Goal: Do amazing research and produce summaries based on the content you are working with
Backstory: You're a Principal Researcher at a big company and you need to do a research about a given topic.
{{task}}
```

```python
import json
import os
import requests

def partition_html(text=""):
    return text

async def scrape_and_summarize_website(website):
    """Useful to scrape and summarize a website content"""
    url = f"https://chrome.browserless.io/content?token={os.environ['BROWSERLESS_API_KEY']}"
    payload = json.dumps({"url": website})
    headers = {'cache-control': 'no-cache', 'content-type': 'application/json'}
    response = requests.request("POST", url, headers=headers, data=payload)
    elements = partition_html(text=response.text)
    content = "\n\n".join([str(el) for el in elements])
    content = [content[i:i + 8000] for i in range(0, len(content), 8000)]
    summaries = []
    for chunk in content:
        summary = await scrape_and_summarize_website_researcher(
            task=f'Analyze and summarize the content bellow, make sure to include the most relevant information in the summary, return only the summary nothing else.\n\nCONTENT\n----------\n{chunk}')
        summaries.append(summary)
    return "\n\n".join(summaries)
```

```python
def calculate(operation):
    """Useful to perform any mathematical calculations, 
    like sum, minus, multiplication, division, etc.
    The input to this tool should be a mathematical 
    expression, a couple examples are `200*7` or `5000/2*10`
    """
    try:
        return eval(operation)
    except SyntaxError:
        return "Error: Invalid syntax in mathematical expression"
```

```python
import json
import os
import requests

def search_internet(query):
    """Useful to search the internet
    about a given topic and return relevant results"""
    top_result_to_return = 4
    url = "https://google.serper.dev/search"
    payload = json.dumps({"q": query})
    headers = {
        'X-API-KEY': os.environ['SERPER_API_KEY'],
        'content-type': 'application/json'
    }
    response = requests.request("POST", url, headers=headers, data=payload)
    # check if there is an organic key
    if 'organic' not in response.json():
        return "Sorry, I couldn't find anything about that, there could be an error with you serper api key."
    else:
        results = response.json()['organic']
        string = []
        for result in results[:top_result_to_return]:
            try:
                string.append('\n'.join([
                    f"Title: {result['title']}", f"Link: {result['link']}",
                    f"Snippet: {result['snippet']}", "\n-----------------"
                ]))
            except KeyError:
                next

        return '\n'.join(string)
```


```prompt (city_selection_expert)
---
model: gpt-3.5-turbo
import:
    - search_internet
    - scrape_and_summarize_website
---
Role: City Selection Expert
Goal: Select the best city based on weather, season, and prices
Backstory: An expert in analyzing travel data to pick ideal destinations
```

```prompt (local_expert)
---
model: gpt-3.5-turbo
import:
    - search_internet
    - scrape_and_summarize_website
---
Role: Local Expert at this city
Goal: Provide the BEST insights about the selected city
Backstory: A knowledgable local guide with extensive information about the city, it's attactions and customs.
```


```prompt (travel_concierge)
---
model: gpt-3.5-turbo
import:
    - search_internet
    - scrape_and_summarize_website
    - calculate
---
Role: Amazing Travel Concierge
Goal: Create the most amazing travel iterinaries within budget and packing suggestions for the city
Backstory: Specialist in travel planning an logistics with decades of experience
```

```javascript
const { Hono } = require('hono');
const bodyParser = require('body-parser');

const app = new Hono();

// Middleware to parse form data
app.use('/submit', bodyParser.urlencoded({ extended: true }));

app.get('/', (c) => {
  const form = `
    <h1>Welcome to Trip Planner Crew</h1>
    <form action="/submit" method="post">
      <label>From where will you be traveling from?</label><br>
      <input type="text" name="origin"><br>
      <label>What are the cities options you are interested in visiting?</label><br>
      <input type="text" name="cities"><br>
      <label>What is the date range you are interested in traveling?</label><br>
      <input type="text" name="date_range"><br>
      <label>What are some of your high level interests and hobbies?</label><br>
      <input type="text" name="interests"><br>
      <button type="submit">Submit</button>
    </form>
  `;
  return c.html(form);
});

app.post('/submit', (c) => {
  const { origin, cities, date_range, interests } = c.req.body;
  const response = `
    <h2>Trip Details</h2>
    <p><strong>Origin:</strong> ${origin}</p>
    <p><strong>Cities:</strong> ${cities}</p>
    <p><strong>Date Range:</strong> ${date_range}</p>
    <p><strong>Interests:</strong> ${interests}</p>
  `;
  return c.html(response);
});

app.listen(3000, () => {
  console.log('Server running on http://localhost:3000');
});
```