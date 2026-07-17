// fast-toml ships no type declarations.
declare module "fast-toml" {
  const TOML: { parse(input: string): unknown };
  export default TOML;
}
