const {Chidori, GraphBuilder} = require("../../package_node");


test('adds 1 + 2 to equal 3', () => {
    const gb = new GraphBuilder();
    expect(gb.denoCodeNode({name: "test", code: "return 1+1"})).toEqual({"nh": {}});
});

test('nodehandle query', async () => {
    const chi = new Chidori("1", "http://localhost:9800");
    await chi.startServer();
    const gb = new GraphBuilder();
    const nh = gb.denoCodeNode({name: "test", code: "return 1+1"});
    // expect(await nh.query(0, 0)).toEqual({});
});
