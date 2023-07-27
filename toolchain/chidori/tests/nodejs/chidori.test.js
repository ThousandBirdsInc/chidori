const {Chidori, GraphBuilder} = require("../..");

async function delay(ms) {
    // Returns a promise that resolves after "ms" milliseconds
    return new Promise(resolve => setTimeout(resolve, ms));
}


test('initialize without error', () => {
    expect(new Chidori("1", "http://localhost:9800")).toEqual({"chi": {}});
});

test('start server', async () => {
    const chi = new Chidori("21", "http://127.0.0.1:9800");
    await chi.startServer();
    await chi.play(0,0);
    const g = new GraphBuilder();
    g.denoCodeNode({
        name: "InspirationalQuote",
        code: `return {"promptResult": "Believe"}`,
        output: `type InspirationalQuote { promptResult: String }`,
        is_template: true
    });
    g.denoCodeNode({
        name: "CodeNode",
        queries: ["query Q { InspirationalQuote { promptResult } }"],
        code: `return {"output": "Here is your quote for " + \`{{InspirationalQuote.promptResult}}\` }`,
        output: `type CodeNode { output: String }`,
        is_template: true
    });
    g.commit(chi, 0);
    await delay(1000);
    console.log(await chi.graphStructure(0))
    // expect((await chi.query(`
    //     query Q { InspirationalQuote { promptResult } }
    // `, 0, 100))["values"].length).toBe(1)
    // console.log(await chi.query(`
    //     query Q { CodeNode { output } }
    // `, 0, 100))
});

