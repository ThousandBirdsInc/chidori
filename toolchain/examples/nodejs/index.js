const axios = require('axios');
const {Chidori, GraphBuilder} = require("@1kbirds/chidori");

class Story {
    constructor(title, url, score) {
        this.title = title;
        this.url = url;
        this.score = score;
    }
}

const HN_URL_TOP_STORIES = "https://hacker-news.firebaseio.com/v0/topstories.json?print=pretty";

function fetchStory(id) {
    return axios.get(`https://hacker-news.firebaseio.com/v0/item/${id}.json?print=pretty`)
        .then(response => response.data);
}

function fetchHN() {
    return axios.get(HN_URL_TOP_STORIES)
        .then(response => {
            const storyIds = response.data;
            const tasks = storyIds.slice(0, 30).map(id => fetchStory(id));  // Limit to 30 stories
            return Promise.all(tasks)
                .then(stories => {
                    return stories.map(story => {
                        const { title, url, score } = story;
                        return new Story(title, url, score);
                    });
                });
        });
}

class ChidoriWorker {
    constructor() {
        this.c = new Chidori("0", "http://localhost:9800");  // Assuming this is a connection object, replaced with an empty object for now
    }

    async buildGraph() {
        const g = new GraphBuilder();

        const h = g.customNode({
            name: "FetchTopHN",
            nodeTypeName: "FetchTopHN",
            output: "type FetchTopHN { output: String }"
        });

        const hInterpret = g.promptNode({
            name: "InterpretTheGroup",
            template: `
                Based on the following list of HackerNews threads,
                filter this list to only launches of new AI projects: {{FetchTopHN.output}}
            `
        });
        hInterpret.runWhen(g, h);

        const hFormatAndRank = g.promptNode({
            name: "FormatAndRank",
            template: `
                Format this list of new AI projects in markdown, ranking the most 
                interesting projects from most interesting to least. 
                
                {{InterpretTheGroup.promptResult}}
            `
        });
        hFormatAndRank.runWhen(g, hInterpret);

        await g.commit(this.c, 0)
    }

    async run() {
        // Construct the agent graph
        await this.buildGraph();

        // Start graph execution from the root
        // Implement the functionality of the play function
        await this.c.play(0, 0);

        // Run the node execution loop
        // Implement the functionality of the run_custom_node_loop function
        await this.c.runCustomNodeLoop()
    }
}


async function handleFetchHN(nodeWillExec, cb) {
    const stories = await fetchHN();
    // return JSON.stringify(stories);
    return cb({ "output": JSON.stringify(stories) });
    // return ;
}

async function main() {
    let w = new ChidoriWorker();
    await w.c.startServer(":memory:")
    await w.c.registerCustomNodeHandle("FetchTopHN", handleFetchHN);
    await w.run()
}


main();
