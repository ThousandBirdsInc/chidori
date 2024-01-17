import * as c from 'chidori_prompt_format'
// @ts-ignore
import * as j from '@types/jest';

describe('chidoriPromptFormat', () => {
  it('should return a string', () => {
    // @ts-ignore
    expect(c.extract_roles_from_template(`
{{#system}}You are a helpful assistant.{{value}}{{/system}}
{{#user}}test{{/user}}
{{#assistant}}test{{/assistant}}
    `).map(x => x.get_source())).toBe('test')
  })
});
