### Build

```
yarn run build
```

### Test

To test the wasm package directly
```
wasm-pack test --headless --firefox
```

To test the UX of the JS interface
```
yarn run build-local
yarn run test-js
```

### Publish to NPM 

```
cd pkg && npm publish --access=public
```
