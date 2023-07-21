const {simpleFun, NodeHandle} = require("../..");
const {Chidori} = require("../../package_node");

test('sdk', () => {
    expect(simpleFun("1")).toBe("1");
});

test('adds 1 + 2 to equal 3', () => {
    expect(new NodeHandle()).toEqual({"nh": {}});
});

test('nodehandle query', async () => {
    const chi = new Chidori("1", "http://localhost:9800");
    await chi.startServer();
    const nh = new NodeHandle();
    expect(await nh.query(0, 0)).toEqual({});
});
