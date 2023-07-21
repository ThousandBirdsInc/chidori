"use strict";

const { promisify } = require("util");

const {
    simpleFun,
    nodehandleDebugExample,
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
    chidoriCustomNode,
    chidoriDenoCodeNode,
    chidoriVectorMemoryNode
} = require("./native/chidori.node");



// Wrapper class for the boxed `Database` for idiomatic JavaScript usage
class NodeHandle {
    constructor() {
        this.nh = nodehandleDebugExample();
    }

    runWhen(otherNodeHandle) {
        return nodehandleRunWhen.call(this.nh, otherNodeHandle);
    }

    query(branch, frame) {
        return nodehandleQuery.call(this.nh, branch, frame);
    }
}


class Chidori {
    constructor(fileId, url) {
        this.chi = chidoriNew(fileId, url);
    }

    startServer() {
        return chidoriStartServer.call(this.chi);
    }

    objectInterface(executionStatus) {
        return chidoriObjInterface.call(this.chi, executionStatus);
    }

    play(branch, frame) {
        return chidoriPlay.call(this.chi, branch, frame);
    }

    pause() {
        return chidoriPause.call(this.chi, branch, frame);
    }

    query(query, branch, frame) {
        return chidoriQuery.call(this.chi, query, branch, frame)
    }

    branch() {
        return chidoriBranch.call(this.chi, branch, frame);
    }

    graphStructure() {
        return chidoriGraphStructure.call(this.chi);
    }

    objInterface() {
        return chidoriObjInterface.call(this.chi, branch, frame);
    }

    customNode(customNodeCreateOpts) {
        return chidoriCustomNode.call(this.chi, createCustomNodeOpts);
    }

    denoCodeNode(denoCodeNodeCreateOpts) {
        return chidoriDenoCodeNode.call(this.chi, denoCodeNodeCreateOpts);
    }

    vectorMemoryNode(vectorMemoryNodeCreateOpts) {
        return chidoriVectorMemoryNode.call(this.chi, vectorMemoryNodeCreateOpts);
    }
}


module.exports = {
    Chidori: Chidori,
    NodeHandle: NodeHandle,
    simpleFun: simpleFun
};