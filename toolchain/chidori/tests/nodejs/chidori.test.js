const {Chidori} = require("../..");

async function delay(ms) {
    // Returns a promise that resolves after "ms" milliseconds
    return new Promise(resolve => setTimeout(resolve, ms));
}


test('initialize without error', () => {
    expect(new Chidori("1", "localhost:9800")).toEqual({"chi": {}});
});

test('coerce to and from structure', async () => {
    const chi = new Chidori("1", "http://localhost:9800");
    chi.startServer()
    expect(chi.objectInterface({
        "id": "1",
        "monotonic_counter": 0,
        "branch": 0
    })).toEqual({
        "id": "1",
        "monotonic_counter": 0,
        "branch": 0
    });
});


test('start server', async () => {
    const chi = new Chidori("21", "http://127.0.0.1:9800");
    await chi.startServer();
    await chi.play(0,0);
    await chi.denoCodeNode({
        name: "InspirationalQuote",
        code: `return {"promptResult": "Believe"}`,
        output: `type InspirationalQuote { promptResult: String }`,
        is_template: true
    });
    await chi.denoCodeNode({
        name: "CodeNode",
        queries: ["query Q { InspirationalQuote { promptResult } }"],
        code: `return {"output": "Here is your quote for " + \`{{InspirationalQuote.promptResult}}\` }`,
        output: `type CodeNode { output: String }`,
        is_template: true
    });
    await delay(1000);
    console.log(await chi.graphStructure())
    expect((await chi.query(`
        query Q { InspirationalQuote { promptResult } }
    `, 0, 100))["values"].length).toBe(1)
    console.log(await chi.query(`
        query Q { CodeNode { output } }
    `, 0, 100))
});

