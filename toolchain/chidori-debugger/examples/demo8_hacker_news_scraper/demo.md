# This agent demonstrate using axios to fetch hacker news articles, filter and rank them


```javascript (load_hacker_news)
const axios = require('https://deno.land/x/axiod/mod.ts');

const HN_URL_TOP_STORIES = "https://hacker-news.firebaseio.com/v0/topstories.json";

function fetchStory(id) {
    return axios.get(`https://hacker-news.firebaseio.com/v0/item/${id}.json?print=pretty`)
        .then(response => response.data);
}

async function fetchHN() {
    const stories = await axios.get(HN_URL_TOP_STORIES);
    const storyIds = stories.data;
    // only the first 30 
    const tasks = storyIds.slice(0, 30).map(id => fetchStory(id));
    return Promise.all(tasks)
        .then(stories => {
            return stories.map(story => {
                const { title, url, score } = story;
                return {title, url, score};
            });
        });
}
```

Prompt "interpret_the_group"
```prompt (interpret_the_group)
  Based on the following list of HackerNews threads,
  filter this list to only launches of 
  new AI projects: {{fetched_articles}}
```

Prompt "format_and_rank"
```prompt (format_and_rank)
Format this list of new AI projects in markdown, ranking the most 
interesting projects from most interesting to least. 
{{interpret_the_group}}
```

Using a python cell as our entrypoint, demonstrating inter-language execution:
```python
articles = await fetchHN()
format_and_rank(articles=articles)
```






