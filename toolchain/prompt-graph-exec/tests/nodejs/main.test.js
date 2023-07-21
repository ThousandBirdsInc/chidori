const PromptGraphExec = require("../../index.node");

test('adds 1 + 2 to equal 3', () => {
    expect(PromptGraphExec.startServer("9800")).toBe(1);
});
