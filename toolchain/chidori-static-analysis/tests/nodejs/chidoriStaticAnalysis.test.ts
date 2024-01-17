// @ts-ignore
import * as j from '@types/jest';
import {extract_dependencies_python} from "chidori_static_analysis";

describe('chidoriStaticAnalysis', () => {
  it('should return a string', () => {
    // @ts-ignore
    expect(extract_dependencies_python(`
@ch.p(create_dockerfile)
def setup_pipeline(x):
    return x
    `)).toStrictEqual([[{"InFunction": "setup_pipeline"}, {"InFunctionDecorator": 0}, "InCallExpression", {"IdentifierReferredTo": ["create_dockerfile", false]}], [{"InFunction": "setup_pipeline"}, {"InFunctionDecorator": 0}, "InCallExpression", {"Attribute": "p"}, "ChName"], [{"InFunction": "setup_pipeline"}, {"IdentifierReferredTo": ["x", true]}]])
  })
});
