import {SerializedValue} from "@/protobufs/DSL_v1";
import MarkdownPreview from "@/components/markdown";

export const DisplaySerializedValue = (props: {value: SerializedValue | undefined}) => {
  const {value} = props;
  if (!value) {
    return <div>Undefined</div>
  }
  if (value.boolean) {
    return <div>{value.boolean}</div>
  }

  if (value.string) {
    return <MarkdownPreview text={value.string}/>
  }

  if (value.number) {
    return <div>{value.number}</div>
  }

  if (value.array) {
    return <div>{JSON.stringify(value.array)}</div>
  }

  if (value.object) {
    return <div>{JSON.stringify(value.object)}</div>
  }

  if (value.float) {
    return <div>{value.float}</div>
  }

  return <div>
    Unknown type
  </div>
}
