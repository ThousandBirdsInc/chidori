// JSON.stringify / JSON.parse round-trips over a nested object — exercises the
// serializer, parser, and the object/array allocation paths.
const N = 20_000;
const obj = {
  id: 0,
  name: "widget",
  tags: ["a", "b", "c"],
  nested: { x: 1, y: 2, z: { deep: true, items: [1, 2, 3, 4, 5] } },
  flag: false,
};
let acc = 0;
for (let i = 0; i < N; i++) {
  obj.id = i;
  const s = JSON.stringify(obj);
  const back = JSON.parse(s);
  acc += back.id + back.nested.z.items.length + s.length;
}
console.log("RESULT=" + acc);
