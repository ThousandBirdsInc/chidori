"use strict";

const {
    simpleFun,
    nodehandleRunWhen,
    nodehandleQuery,
    chidoriNew,
    chidoriStartServer,
    chidoriObjInterface,
    chidoriPlay,
    chidoriPause,
    chidoriBranch,
    chidoriQuery,
    chidoriGraphStructure,
    chidoriRegisterCustomNodeHandle,
    chidoriRunCustomNodeLoop,
    graphbuilderNew,
    graphbuilderCustomNode,
    graphbuilderPromptNode,
    graphbuilderDenoCodeNode,
    graphbuilderVectorMemoryNode,
    graphbuilderCommit
} = require("./native/chidori.node");

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

class NodeHandle {
    constructor(nh) {
        this.nh = nh;
    }

    runWhen(graphBuilder, otherNodeHandle) {
        return nodehandleRunWhen.call(this.nh, graphBuilder.g, otherNodeHandle.nh);
    }

    query(branch, frame) {
        return nodehandleQuery.call(this.nh, branch, frame);
    }
}


class Chidori {
    constructor(fileId, url) {
        this.chi = chidoriNew(fileId, url);
    }

    startServer(filePath) {
        return chidoriStartServer.call(this.chi, filePath);
    }

    objectInterface(executionStatus) {
        return chidoriObjInterface.call(this.chi, executionStatus);
    }

    play(branch, frame) {
        return chidoriPlay.call(this.chi, branch, frame);
    }

    pause(branch, frame) {
        return chidoriPause.call(this.chi, branch, frame);
    }

    query(query, branch, frame) {
        return chidoriQuery.call(this.chi, query, branch, frame)
    }

    branch(branch, frame) {
        return chidoriBranch.call(this.chi, branch, frame);
    }

    graphStructure(branch) {
        return chidoriGraphStructure.call(this.chi, branch);
    }

    registerCustomNodeHandle(nodeTypeName, handle) {
        // TODO: we actually pass a callback to the function provided by the user, which they invoke with their result
        return chidoriRegisterCustomNodeHandle.call(this.chi, nodeTypeName, handle);
    }

    runCustomNodeLoop() {
        return chidoriRunCustomNodeLoop.call(this.chi);
    }

}

class GraphBuilder {
    constructor() {
        this.g = graphbuilderNew();
    }

    customNode(createCustomNodeOpts) {
        return new NodeHandle(graphbuilderCustomNode.call(this.g, transformKeys(createCustomNodeOpts)));
    }

    promptNode(promptNodeCreateOpts) {
        return new NodeHandle(graphbuilderPromptNode.call(this.g, transformKeys(promptNodeCreateOpts)));
    }

    denoCodeNode(denoCodeNodeCreateOpts) {
        return new NodeHandle(graphbuilderDenoCodeNode.call(this.g, transformKeys(denoCodeNodeCreateOpts)));
    }

    vectorMemoryNode(vectorMemoryNodeCreateOpts) {
        return new NodeHandle(graphbuilderVectorMemoryNode.call(this.g, transformKeys(vectorMemoryNodeCreateOpts)));
    }

    commit(chidori) {
        return graphbuilderCommit.call(this.g, chidori.chi, 0);
    }
}


module.exports = {
    Chidori: Chidori,
    GraphBuilder: GraphBuilder,
    NodeHandle: NodeHandle,
    simpleFun: simpleFun
};