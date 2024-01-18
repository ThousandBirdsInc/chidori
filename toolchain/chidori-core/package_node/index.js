"use strict";

const {
    std_ai_llm_openai_batch,
    std_code_rustpython_source_code_run_python,
} = require("./native/chidori-core.node");

const toSnakeCase = str => str.replace(/[A-Z]/g, letter => `_${letter.toLowerCase()}`);

const transformKeys = (obj) => {
    if (Array.isArray(obj)) {
        return obj.map(val => transformKeys(val));
    } else if (obj !== null && obj.constructor === Object) {
        return Object.keys(obj).reduce((accumulator, key) => {
            accumulator[toSnakeCase(key)] = transformKeys(obj[key]);
            return accumulator;
        }, {});
    }
    return obj;
};



module.exports = {
    std_ai_llm_openai_batch: std_ai_llm_openai_batch,
    std_code_rustpython_source_code_run_python: std_code_rustpython_source_code_run_python
};