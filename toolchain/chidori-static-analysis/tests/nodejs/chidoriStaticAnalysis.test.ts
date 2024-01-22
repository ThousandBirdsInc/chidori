// @ts-ignore
import * as j from '@types/jest';
import {extract_dependencies_python, extract_cell_info} from "chidori_static_analysis";

describe('chidoriStaticAnalysis', () => {
  it('should return a string', () => {
    // @ts-ignore
    expect(extract_dependencies_python(`
@ch.p(create_dockerfile)
def setup_pipeline(x):
    return x
    `)).toStrictEqual([[{"InFunction": "setup_pipeline"},
      {"InFunctionDecorator": 0},
      "InCallExpression",
      {"IdentifierReferredTo": ["create_dockerfile",
          false]}],
      [{"InFunction": "setup_pipeline"},
        {"InFunctionDecorator": 0},
        "InCallExpression",
        {"Attribute": "p"},
        "ChName"],
      [{"InFunction": "setup_pipeline"},
        {"IdentifierReferredTo": ["x",
            true]}]])
  })

  it('should create a useful report', () => {
    // @ts-ignore
    expect(extract_cell_info(`
@ch.p(create_dockerfile)
def setup_pipeline(x):
    return x
y = 20
    `))
      .toStrictEqual({
        cell_depended_values: new Map([
          ["create_dockerfile", {
            context_path: [
              {InFunction: "setup_pipeline"},
              {InFunctionDecorator: 0},
              "InCallExpression",
              {IdentifierReferredTo: ["create_dockerfile", false]}
            ]
          }]
        ]),
        cell_exposed_values: new Map([
          ["y", {
            context_path: [
              "AssignmentToStatement",
              {IdentifierReferredTo: ["y", false]}
            ]
          }]
        ]),
        triggerable_functions: new Map([
          ["setup_pipeline", {
            context_path: [
              {InFunction: "setup_pipeline"}
            ],
            emit_event: [],
            trigger_on: []
          }]
        ])
      });
  });
});
