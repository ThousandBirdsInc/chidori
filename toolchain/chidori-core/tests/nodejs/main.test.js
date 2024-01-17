const {} = require("../..");

async function delay(ms) {
    // Returns a promise that resolves after "ms" milliseconds
    return new Promise(resolve => setTimeout(resolve, ms));
}

test('initialize without error', () => {
    expect(true).toEqual(true);
});

