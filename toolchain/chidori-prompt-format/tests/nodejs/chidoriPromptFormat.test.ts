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
    `)).toStrictEqual([{role: "System", source: 'You are a helpful assistant.{{value}}'}, {role: "User", source: 'test'}, {role: "Assistant", source: 'test'}])
  })
});


describe('chidoriPromptFormatRendering', () => {
    it('should return a string', () => {
      // @ts-ignore
      expect(c.render_template_prompt(`Basic template {{user.name}}`, {user: {name: "example"}}, {}))
        .toBe('Basic template example')
    });

  it('should return a string', () => {
    // @ts-ignore
    const roles = c.extract_roles_from_template(`
{{#system}}You are a helpful assistant.{{value}}{{/system}}
{{#user}}test{{/user}}
{{#assistant}}test{{/assistant}}`)
    // @ts-ignore
    expect(c.render_template_prompt(roles[0].source, {value: "testing"}, {})).toBe('You are a helpful assistant.testing')
  });
});
