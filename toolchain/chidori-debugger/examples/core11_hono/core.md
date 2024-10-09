# Demonstrating running a hono web service to produce a user facing interface


```python
def testingFunC(x):
    return x + "Hello"
```

```javascript
import { Hono } from 'https://deno.land/x/hono/mod.ts';
import { serve } from 'https://deno.land/std@0.145.0/http/server.ts';


const app = new Hono();



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

app.post('/submit', async (c) => {
  const body = await c.req.parseBody();
  const { origin, cities, date_range, interests } = body;
  const xx = await testingFunC(origin);
  const response = `
    <h2>Trip Details</h2>
    <p><strong>Origin:</strong> ${xx}</p>
    <p><strong>Cities:</strong> ${cities}</p>
    <p><strong>Date Range:</strong> ${date_range}</p>
    <p><strong>Interests:</strong> ${interests}</p>
  `;
  return c.html(response);
});

serve(app.fetch);
```