const {std_code_rustpython_source_code_run_python} = require("../..");

async function delay(ms) {
    // Returns a promise that resolves after "ms" milliseconds
    return new Promise(resolve => setTimeout(resolve, ms));
}

test('initialize without error', () => {
    expect(std_code_rustpython_source_code_run_python("x = 2+2")).toEqual({"x": 4});
});

