/* eslint-disable */
import Long from "long";
import _m0 from "protobufjs/minimal";
import { Observable } from "rxjs";
import { map } from "rxjs/operators";

export const protobufPackage = "promptgraph";

export enum SupportedChatModel {
  GPT_4 = 0,
  GPT_4_0314 = 1,
  GPT_4_32K = 2,
  GPT_4_32K_0314 = 3,
  GPT_3_5_TURBO = 4,
  GPT_3_5_TURBO_0301 = 5,
  UNRECOGNIZED = -1,
}

export function supportedChatModelFromJSON(object: any): SupportedChatModel {
  switch (object) {
    case 0:
    case "GPT_4":
      return SupportedChatModel.GPT_4;
    case 1:
    case "GPT_4_0314":
      return SupportedChatModel.GPT_4_0314;
    case 2:
    case "GPT_4_32K":
      return SupportedChatModel.GPT_4_32K;
    case 3:
    case "GPT_4_32K_0314":
      return SupportedChatModel.GPT_4_32K_0314;
    case 4:
    case "GPT_3_5_TURBO":
      return SupportedChatModel.GPT_3_5_TURBO;
    case 5:
    case "GPT_3_5_TURBO_0301":
      return SupportedChatModel.GPT_3_5_TURBO_0301;
    case -1:
    case "UNRECOGNIZED":
    default:
      return SupportedChatModel.UNRECOGNIZED;
  }
}

export function supportedChatModelToJSON(object: SupportedChatModel): string {
  switch (object) {
    case SupportedChatModel.GPT_4:
      return "GPT_4";
    case SupportedChatModel.GPT_4_0314:
      return "GPT_4_0314";
    case SupportedChatModel.GPT_4_32K:
      return "GPT_4_32K";
    case SupportedChatModel.GPT_4_32K_0314:
      return "GPT_4_32K_0314";
    case SupportedChatModel.GPT_3_5_TURBO:
      return "GPT_3_5_TURBO";
    case SupportedChatModel.GPT_3_5_TURBO_0301:
      return "GPT_3_5_TURBO_0301";
    case SupportedChatModel.UNRECOGNIZED:
    default:
      return "UNRECOGNIZED";
  }
}

export enum SupportedCompletionModel {
  TEXT_DAVINCI_003 = 0,
  TEXT_DAVINCI_002 = 1,
  TEXT_CURIE_001 = 2,
  TEXT_BABBAGE_001 = 3,
  TEXT_ADA_00 = 4,
  UNRECOGNIZED = -1,
}

export function supportedCompletionModelFromJSON(object: any): SupportedCompletionModel {
  switch (object) {
    case 0:
    case "TEXT_DAVINCI_003":
      return SupportedCompletionModel.TEXT_DAVINCI_003;
    case 1:
    case "TEXT_DAVINCI_002":
      return SupportedCompletionModel.TEXT_DAVINCI_002;
    case 2:
    case "TEXT_CURIE_001":
      return SupportedCompletionModel.TEXT_CURIE_001;
    case 3:
    case "TEXT_BABBAGE_001":
      return SupportedCompletionModel.TEXT_BABBAGE_001;
    case 4:
    case "TEXT_ADA_00":
      return SupportedCompletionModel.TEXT_ADA_00;
    case -1:
    case "UNRECOGNIZED":
    default:
      return SupportedCompletionModel.UNRECOGNIZED;
  }
}

export function supportedCompletionModelToJSON(object: SupportedCompletionModel): string {
  switch (object) {
    case SupportedCompletionModel.TEXT_DAVINCI_003:
      return "TEXT_DAVINCI_003";
    case SupportedCompletionModel.TEXT_DAVINCI_002:
      return "TEXT_DAVINCI_002";
    case SupportedCompletionModel.TEXT_CURIE_001:
      return "TEXT_CURIE_001";
    case SupportedCompletionModel.TEXT_BABBAGE_001:
      return "TEXT_BABBAGE_001";
    case SupportedCompletionModel.TEXT_ADA_00:
      return "TEXT_ADA_00";
    case SupportedCompletionModel.UNRECOGNIZED:
    default:
      return "UNRECOGNIZED";
  }
}

export enum SupportedEmebddingModel {
  TEXT_EMBEDDING_ADA_002 = 0,
  TEXT_SEARCH_ADA_DOC_001 = 1,
  UNRECOGNIZED = -1,
}

export function supportedEmebddingModelFromJSON(object: any): SupportedEmebddingModel {
  switch (object) {
    case 0:
    case "TEXT_EMBEDDING_ADA_002":
      return SupportedEmebddingModel.TEXT_EMBEDDING_ADA_002;
    case 1:
    case "TEXT_SEARCH_ADA_DOC_001":
      return SupportedEmebddingModel.TEXT_SEARCH_ADA_DOC_001;
    case -1:
    case "UNRECOGNIZED":
    default:
      return SupportedEmebddingModel.UNRECOGNIZED;
  }
}

export function supportedEmebddingModelToJSON(object: SupportedEmebddingModel): string {
  switch (object) {
    case SupportedEmebddingModel.TEXT_EMBEDDING_ADA_002:
      return "TEXT_EMBEDDING_ADA_002";
    case SupportedEmebddingModel.TEXT_SEARCH_ADA_DOC_001:
      return "TEXT_SEARCH_ADA_DOC_001";
    case SupportedEmebddingModel.UNRECOGNIZED:
    default:
      return "UNRECOGNIZED";
  }
}

export enum SupportedVectorDatabase {
  IN_MEMORY = 0,
  CHROMA = 1,
  PINECONEDB = 2,
  QDRANT = 3,
  UNRECOGNIZED = -1,
}

export function supportedVectorDatabaseFromJSON(object: any): SupportedVectorDatabase {
  switch (object) {
    case 0:
    case "IN_MEMORY":
      return SupportedVectorDatabase.IN_MEMORY;
    case 1:
    case "CHROMA":
      return SupportedVectorDatabase.CHROMA;
    case 2:
    case "PINECONEDB":
      return SupportedVectorDatabase.PINECONEDB;
    case 3:
    case "QDRANT":
      return SupportedVectorDatabase.QDRANT;
    case -1:
    case "UNRECOGNIZED":
    default:
      return SupportedVectorDatabase.UNRECOGNIZED;
  }
}

export function supportedVectorDatabaseToJSON(object: SupportedVectorDatabase): string {
  switch (object) {
    case SupportedVectorDatabase.IN_MEMORY:
      return "IN_MEMORY";
    case SupportedVectorDatabase.CHROMA:
      return "CHROMA";
    case SupportedVectorDatabase.PINECONEDB:
      return "PINECONEDB";
    case SupportedVectorDatabase.QDRANT:
      return "QDRANT";
    case SupportedVectorDatabase.UNRECOGNIZED:
    default:
      return "UNRECOGNIZED";
  }
}

export enum SupportedSourceCodeLanguages {
  DENO = 0,
  STARLARK = 1,
  UNRECOGNIZED = -1,
}

export function supportedSourceCodeLanguagesFromJSON(object: any): SupportedSourceCodeLanguages {
  switch (object) {
    case 0:
    case "DENO":
      return SupportedSourceCodeLanguages.DENO;
    case 1:
    case "STARLARK":
      return SupportedSourceCodeLanguages.STARLARK;
    case -1:
    case "UNRECOGNIZED":
    default:
      return SupportedSourceCodeLanguages.UNRECOGNIZED;
  }
}

export function supportedSourceCodeLanguagesToJSON(object: SupportedSourceCodeLanguages): string {
  switch (object) {
    case SupportedSourceCodeLanguages.DENO:
      return "DENO";
    case SupportedSourceCodeLanguages.STARLARK:
      return "STARLARK";
    case SupportedSourceCodeLanguages.UNRECOGNIZED:
    default:
      return "UNRECOGNIZED";
  }
}

export enum MemoryAction {
  READ = 0,
  WRITE = 1,
  DELETE = 2,
  UNRECOGNIZED = -1,
}

export function memoryActionFromJSON(object: any): MemoryAction {
  switch (object) {
    case 0:
    case "READ":
      return MemoryAction.READ;
    case 1:
    case "WRITE":
      return MemoryAction.WRITE;
    case 2:
    case "DELETE":
      return MemoryAction.DELETE;
    case -1:
    case "UNRECOGNIZED":
    default:
      return MemoryAction.UNRECOGNIZED;
  }
}

export function memoryActionToJSON(object: MemoryAction): string {
  switch (object) {
    case MemoryAction.READ:
      return "READ";
    case MemoryAction.WRITE:
      return "WRITE";
    case MemoryAction.DELETE:
      return "DELETE";
    case MemoryAction.UNRECOGNIZED:
    default:
      return "UNRECOGNIZED";
  }
}

export interface Query {
  query?: string | undefined;
}

/** Processed version of the Query */
export interface QueryPaths {
  node: string;
  path: Path[];
}

export interface OutputType {
  output: string;
}

/** Processed version of the OutputType */
export interface OutputPaths {
  node: string;
  path: Path[];
}

/**
 * Alias is a reference to another node, any value set
 * on this node will propagate for the alias as well
 */
export interface PromptGraphAlias {
  from: string;
  to: string;
}

export interface PromptGraphConstant {
  value: SerializedValue | undefined;
}

export interface PromptGraphVar {
}

export interface PromptGraphOutputValue {
}

export interface PromptGraphNodeCodeSourceCode {
  language: SupportedSourceCodeLanguages;
  sourceCode: string;
  template: boolean;
}

export interface PromptGraphParameterNode {
}

export interface PromptGraphMap {
  path: string;
}

export interface PromptGraphNodeCode {
  sourceCode?: PromptGraphNodeCodeSourceCode | undefined;
  zipfile?: Uint8Array | undefined;
  s3Path?: string | undefined;
}

export interface PromptGraphNodeLoader {
  /** Load a zip file, decompress it, and make the paths available as keys */
  zipfileBytes?: Uint8Array | undefined;
}

export interface PromptGraphNodeCustom {
  typeName: string;
}

/**
 * TODO: we should allow the user to freely manipulate wall-clock time
 * Output value of this should just be the timestamp
 */
export interface PromptGraphNodeSchedule {
  crontab?: string | undefined;
  naturalLanguage?: string | undefined;
  everyMs?: string | undefined;
}

export interface PromptGraphNodePrompt {
  template: string;
  chatModel?: SupportedChatModel | undefined;
  completionModel?: SupportedCompletionModel | undefined;
  temperature: number;
  topP: number;
  maxTokens: number;
  presencePenalty: number;
  frequencyPenalty: number;
  /**
   * TODO: set the user token
   * TODO: support logit bias
   */
  stop: string[];
}

/**
 * TODO: this expects a selector for the query? - no its a template and you build that
 * TODO: what about the output type? pre-defined
 * TODO: what about the metadata?
 * TODO: metadata could be an independent query, or it could instead be a template too
 */
export interface PromptGraphNodeMemory {
  collectionName: string;
  template: string;
  model?: SupportedEmebddingModel | undefined;
  db?: SupportedVectorDatabase | undefined;
  action: MemoryAction;
}

export interface PromptGraphNodeObservation {
  integration: string;
}

export interface PromptGraphNodeComponent {
  inlineFile?: File | undefined;
  bytesReference?: Uint8Array | undefined;
  s3PathReference?: string | undefined;
}

export interface PromptGraphNodeEcho {
}

/** TODO: configure resolving joins */
export interface PromptGraphNodeJoin {
}

export interface ItemCore {
  name: string;
  queries: Query[];
  outputTables: string[];
  output: OutputType | undefined;
}

export interface Item {
  core: ItemCore | undefined;
  alias?: PromptGraphAlias | undefined;
  map?: PromptGraphMap | undefined;
  constant?: PromptGraphConstant | undefined;
  variable?: PromptGraphVar | undefined;
  output?:
    | PromptGraphOutputValue
    | undefined;
  /** TODO: delete above this line */
  nodeCode?: PromptGraphNodeCode | undefined;
  nodePrompt?: PromptGraphNodePrompt | undefined;
  nodeMemory?: PromptGraphNodeMemory | undefined;
  nodeComponent?: PromptGraphNodeComponent | undefined;
  nodeObservation?: PromptGraphNodeObservation | undefined;
  nodeParameter?: PromptGraphParameterNode | undefined;
  nodeEcho?: PromptGraphNodeEcho | undefined;
  nodeLoader?: PromptGraphNodeLoader | undefined;
  nodeCustom?: PromptGraphNodeCustom | undefined;
  nodeJoin?: PromptGraphNodeJoin | undefined;
  nodeSchedule?: PromptGraphNodeSchedule | undefined;
}

/** TODO: add a flag for 'Cleaned', 'Dirty', 'Validated' */
export interface File {
  id: string;
  nodes: Item[];
}

export interface Path {
  address: string[];
}

export interface TypeDefinition {
  primitive?: PrimitiveType | undefined;
  array?: ArrayType | undefined;
  object?: ObjectType | undefined;
  union?: UnionType | undefined;
  intersection?: IntersectionType | undefined;
  optional?: OptionalType | undefined;
  enum?: EnumType | undefined;
}

export interface PrimitiveType {
  isString?: boolean | undefined;
  isNumber?: boolean | undefined;
  isBoolean?: boolean | undefined;
  isNull?: boolean | undefined;
  isUndefined?: boolean | undefined;
}

export interface ArrayType {
  type: TypeDefinition | undefined;
}

export interface ObjectType {
  fields: { [key: string]: TypeDefinition };
}

export interface ObjectType_FieldsEntry {
  key: string;
  value: TypeDefinition | undefined;
}

export interface UnionType {
  types: TypeDefinition[];
}

export interface IntersectionType {
  types: TypeDefinition[];
}

export interface OptionalType {
  type: TypeDefinition | undefined;
}

export interface EnumType {
  values: { [key: string]: string };
}

export interface EnumType_ValuesEntry {
  key: string;
  value: string;
}

export interface SerializedValueArray {
  values: SerializedValue[];
}

export interface SerializedValueObject {
  values: { [key: string]: SerializedValue };
}

export interface SerializedValueObject_ValuesEntry {
  key: string;
  value: SerializedValue | undefined;
}

export interface SerializedValue {
  float?: number | undefined;
  number?: number | undefined;
  string?: string | undefined;
  boolean?: boolean | undefined;
  array?: SerializedValueArray | undefined;
  object?: SerializedValueObject | undefined;
}

export interface ChangeValue {
  path: Path | undefined;
  value: SerializedValue | undefined;
  branch: number;
}

export interface WrappedChangeValue {
  monotonicCounter: number;
  changeValue: ChangeValue | undefined;
}

/** Computation of a node */
export interface NodeWillExecute {
  sourceNode: string;
  changeValuesUsedInExecution: WrappedChangeValue[];
  matchedQueryIndex: number;
}

/** Group of node computations to run */
export interface DispatchResult {
  operations: NodeWillExecute[];
}

export interface NodeWillExecuteOnBranch {
  branch: number;
  counter: number;
  customNodeTypeName?: string | undefined;
  node: NodeWillExecute | undefined;
}

export interface ChangeValueWithCounter {
  filledValues: ChangeValue[];
  parentMonotonicCounters: number[];
  monotonicCounter: number;
  branch: number;
  sourceNode: string;
}

export interface CounterWithPath {
  monotonicCounter: number;
  path: Path | undefined;
}

/** Input proposals */
export interface InputProposal {
  name: string;
  output: OutputType | undefined;
  counter: number;
  branch: number;
}

export interface RequestInputProposalResponse {
  id: string;
  proposalCounter: number;
  changes: ChangeValue[];
  branch: number;
}

export interface DivergentBranch {
  branch: number;
  divergesAtCounter: number;
}

export interface Branch {
  id: number;
  sourceBranchIds: number[];
  divergentBranches: DivergentBranch[];
  divergesAtCounter: number;
}

export interface Empty {
}

/**
 * This is the return value from api calls that reports the current counter and branch the operation
 * was performed on.
 */
export interface ExecutionStatus {
  id: string;
  monotonicCounter: number;
  branch: number;
}

export interface FileAddressedChangeValueWithCounter {
  id: string;
  nodeName: string;
  branch: number;
  counter: number;
  change: ChangeValueWithCounter | undefined;
}

export interface RequestOnlyId {
  id: string;
  branch: number;
}

export interface FilteredPollNodeWillExecuteEventsRequest {
  id: string;
}

export interface RequestAtFrame {
  id: string;
  frame: number;
  branch: number;
}

export interface RequestNewBranch {
  id: string;
  sourceBranchId: number;
  divergesAtCounter: number;
}

export interface RequestListBranches {
  id: string;
}

export interface ListBranchesRes {
  id: string;
  branches: Branch[];
}

export interface RequestFileMerge {
  id: string;
  file: File | undefined;
  branch: number;
}

export interface ParquetFile {
  data: Uint8Array;
}

export interface QueryAtFrame {
  id: string;
  query: Query | undefined;
  frame: number;
  branch: number;
}

export interface QueryAtFrameResponse {
  values: WrappedChangeValue[];
}

export interface RequestAckNodeWillExecuteEvent {
  id: string;
  branch: number;
  counter: number;
}

export interface RespondPollNodeWillExecuteEvents {
  nodeWillExecuteEvents: NodeWillExecuteOnBranch[];
}

export interface PromptLibraryRecord {
  record: UpsertPromptLibraryRecord | undefined;
  versionCounter: number;
}

export interface UpsertPromptLibraryRecord {
  template: string;
  name: string;
  id: string;
  description?: string | undefined;
}

export interface ListRegisteredGraphsResponse {
  ids: string[];
}

function createBaseQuery(): Query {
  return { query: undefined };
}

export const Query = {
  encode(message: Query, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.query !== undefined) {
      writer.uint32(10).string(message.query);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): Query {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseQuery();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.query = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): Query {
    return { query: isSet(object.query) ? String(object.query) : undefined };
  },

  toJSON(message: Query): unknown {
    const obj: any = {};
    if (message.query !== undefined) {
      obj.query = message.query;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<Query>, I>>(base?: I): Query {
    return Query.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<Query>, I>>(object: I): Query {
    const message = createBaseQuery();
    message.query = object.query ?? undefined;
    return message;
  },
};

function createBaseQueryPaths(): QueryPaths {
  return { node: "", path: [] };
}

export const QueryPaths = {
  encode(message: QueryPaths, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.node !== "") {
      writer.uint32(10).string(message.node);
    }
    for (const v of message.path) {
      Path.encode(v!, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): QueryPaths {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseQueryPaths();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.node = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.path.push(Path.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): QueryPaths {
    return {
      node: isSet(object.node) ? String(object.node) : "",
      path: Array.isArray(object?.path) ? object.path.map((e: any) => Path.fromJSON(e)) : [],
    };
  },

  toJSON(message: QueryPaths): unknown {
    const obj: any = {};
    if (message.node !== "") {
      obj.node = message.node;
    }
    if (message.path?.length) {
      obj.path = message.path.map((e) => Path.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<QueryPaths>, I>>(base?: I): QueryPaths {
    return QueryPaths.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<QueryPaths>, I>>(object: I): QueryPaths {
    const message = createBaseQueryPaths();
    message.node = object.node ?? "";
    message.path = object.path?.map((e) => Path.fromPartial(e)) || [];
    return message;
  },
};

function createBaseOutputType(): OutputType {
  return { output: "" };
}

export const OutputType = {
  encode(message: OutputType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.output !== "") {
      writer.uint32(18).string(message.output);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): OutputType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseOutputType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 2:
          if (tag !== 18) {
            break;
          }

          message.output = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): OutputType {
    return { output: isSet(object.output) ? String(object.output) : "" };
  },

  toJSON(message: OutputType): unknown {
    const obj: any = {};
    if (message.output !== "") {
      obj.output = message.output;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<OutputType>, I>>(base?: I): OutputType {
    return OutputType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<OutputType>, I>>(object: I): OutputType {
    const message = createBaseOutputType();
    message.output = object.output ?? "";
    return message;
  },
};

function createBaseOutputPaths(): OutputPaths {
  return { node: "", path: [] };
}

export const OutputPaths = {
  encode(message: OutputPaths, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.node !== "") {
      writer.uint32(10).string(message.node);
    }
    for (const v of message.path) {
      Path.encode(v!, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): OutputPaths {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseOutputPaths();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.node = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.path.push(Path.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): OutputPaths {
    return {
      node: isSet(object.node) ? String(object.node) : "",
      path: Array.isArray(object?.path) ? object.path.map((e: any) => Path.fromJSON(e)) : [],
    };
  },

  toJSON(message: OutputPaths): unknown {
    const obj: any = {};
    if (message.node !== "") {
      obj.node = message.node;
    }
    if (message.path?.length) {
      obj.path = message.path.map((e) => Path.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<OutputPaths>, I>>(base?: I): OutputPaths {
    return OutputPaths.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<OutputPaths>, I>>(object: I): OutputPaths {
    const message = createBaseOutputPaths();
    message.node = object.node ?? "";
    message.path = object.path?.map((e) => Path.fromPartial(e)) || [];
    return message;
  },
};

function createBasePromptGraphAlias(): PromptGraphAlias {
  return { from: "", to: "" };
}

export const PromptGraphAlias = {
  encode(message: PromptGraphAlias, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.from !== "") {
      writer.uint32(18).string(message.from);
    }
    if (message.to !== "") {
      writer.uint32(26).string(message.to);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphAlias {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphAlias();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 2:
          if (tag !== 18) {
            break;
          }

          message.from = reader.string();
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.to = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphAlias {
    return { from: isSet(object.from) ? String(object.from) : "", to: isSet(object.to) ? String(object.to) : "" };
  },

  toJSON(message: PromptGraphAlias): unknown {
    const obj: any = {};
    if (message.from !== "") {
      obj.from = message.from;
    }
    if (message.to !== "") {
      obj.to = message.to;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphAlias>, I>>(base?: I): PromptGraphAlias {
    return PromptGraphAlias.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphAlias>, I>>(object: I): PromptGraphAlias {
    const message = createBasePromptGraphAlias();
    message.from = object.from ?? "";
    message.to = object.to ?? "";
    return message;
  },
};

function createBasePromptGraphConstant(): PromptGraphConstant {
  return { value: undefined };
}

export const PromptGraphConstant = {
  encode(message: PromptGraphConstant, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.value !== undefined) {
      SerializedValue.encode(message.value, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphConstant {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphConstant();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 2:
          if (tag !== 18) {
            break;
          }

          message.value = SerializedValue.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphConstant {
    return { value: isSet(object.value) ? SerializedValue.fromJSON(object.value) : undefined };
  },

  toJSON(message: PromptGraphConstant): unknown {
    const obj: any = {};
    if (message.value !== undefined) {
      obj.value = SerializedValue.toJSON(message.value);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphConstant>, I>>(base?: I): PromptGraphConstant {
    return PromptGraphConstant.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphConstant>, I>>(object: I): PromptGraphConstant {
    const message = createBasePromptGraphConstant();
    message.value = (object.value !== undefined && object.value !== null)
      ? SerializedValue.fromPartial(object.value)
      : undefined;
    return message;
  },
};

function createBasePromptGraphVar(): PromptGraphVar {
  return {};
}

export const PromptGraphVar = {
  encode(_: PromptGraphVar, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphVar {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphVar();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(_: any): PromptGraphVar {
    return {};
  },

  toJSON(_: PromptGraphVar): unknown {
    const obj: any = {};
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphVar>, I>>(base?: I): PromptGraphVar {
    return PromptGraphVar.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphVar>, I>>(_: I): PromptGraphVar {
    const message = createBasePromptGraphVar();
    return message;
  },
};

function createBasePromptGraphOutputValue(): PromptGraphOutputValue {
  return {};
}

export const PromptGraphOutputValue = {
  encode(_: PromptGraphOutputValue, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphOutputValue {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphOutputValue();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(_: any): PromptGraphOutputValue {
    return {};
  },

  toJSON(_: PromptGraphOutputValue): unknown {
    const obj: any = {};
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphOutputValue>, I>>(base?: I): PromptGraphOutputValue {
    return PromptGraphOutputValue.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphOutputValue>, I>>(_: I): PromptGraphOutputValue {
    const message = createBasePromptGraphOutputValue();
    return message;
  },
};

function createBasePromptGraphNodeCodeSourceCode(): PromptGraphNodeCodeSourceCode {
  return { language: 0, sourceCode: "", template: false };
}

export const PromptGraphNodeCodeSourceCode = {
  encode(message: PromptGraphNodeCodeSourceCode, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.language !== 0) {
      writer.uint32(8).int32(message.language);
    }
    if (message.sourceCode !== "") {
      writer.uint32(18).string(message.sourceCode);
    }
    if (message.template === true) {
      writer.uint32(24).bool(message.template);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeCodeSourceCode {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeCodeSourceCode();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 8) {
            break;
          }

          message.language = reader.int32() as any;
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.sourceCode = reader.string();
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.template = reader.bool();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeCodeSourceCode {
    return {
      language: isSet(object.language) ? supportedSourceCodeLanguagesFromJSON(object.language) : 0,
      sourceCode: isSet(object.sourceCode) ? String(object.sourceCode) : "",
      template: isSet(object.template) ? Boolean(object.template) : false,
    };
  },

  toJSON(message: PromptGraphNodeCodeSourceCode): unknown {
    const obj: any = {};
    if (message.language !== 0) {
      obj.language = supportedSourceCodeLanguagesToJSON(message.language);
    }
    if (message.sourceCode !== "") {
      obj.sourceCode = message.sourceCode;
    }
    if (message.template === true) {
      obj.template = message.template;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeCodeSourceCode>, I>>(base?: I): PromptGraphNodeCodeSourceCode {
    return PromptGraphNodeCodeSourceCode.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeCodeSourceCode>, I>>(
    object: I,
  ): PromptGraphNodeCodeSourceCode {
    const message = createBasePromptGraphNodeCodeSourceCode();
    message.language = object.language ?? 0;
    message.sourceCode = object.sourceCode ?? "";
    message.template = object.template ?? false;
    return message;
  },
};

function createBasePromptGraphParameterNode(): PromptGraphParameterNode {
  return {};
}

export const PromptGraphParameterNode = {
  encode(_: PromptGraphParameterNode, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphParameterNode {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphParameterNode();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(_: any): PromptGraphParameterNode {
    return {};
  },

  toJSON(_: PromptGraphParameterNode): unknown {
    const obj: any = {};
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphParameterNode>, I>>(base?: I): PromptGraphParameterNode {
    return PromptGraphParameterNode.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphParameterNode>, I>>(_: I): PromptGraphParameterNode {
    const message = createBasePromptGraphParameterNode();
    return message;
  },
};

function createBasePromptGraphMap(): PromptGraphMap {
  return { path: "" };
}

export const PromptGraphMap = {
  encode(message: PromptGraphMap, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.path !== "") {
      writer.uint32(34).string(message.path);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphMap {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphMap();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 4:
          if (tag !== 34) {
            break;
          }

          message.path = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphMap {
    return { path: isSet(object.path) ? String(object.path) : "" };
  },

  toJSON(message: PromptGraphMap): unknown {
    const obj: any = {};
    if (message.path !== "") {
      obj.path = message.path;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphMap>, I>>(base?: I): PromptGraphMap {
    return PromptGraphMap.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphMap>, I>>(object: I): PromptGraphMap {
    const message = createBasePromptGraphMap();
    message.path = object.path ?? "";
    return message;
  },
};

function createBasePromptGraphNodeCode(): PromptGraphNodeCode {
  return { sourceCode: undefined, zipfile: undefined, s3Path: undefined };
}

export const PromptGraphNodeCode = {
  encode(message: PromptGraphNodeCode, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.sourceCode !== undefined) {
      PromptGraphNodeCodeSourceCode.encode(message.sourceCode, writer.uint32(50).fork()).ldelim();
    }
    if (message.zipfile !== undefined) {
      writer.uint32(58).bytes(message.zipfile);
    }
    if (message.s3Path !== undefined) {
      writer.uint32(66).string(message.s3Path);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeCode {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeCode();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 6:
          if (tag !== 50) {
            break;
          }

          message.sourceCode = PromptGraphNodeCodeSourceCode.decode(reader, reader.uint32());
          continue;
        case 7:
          if (tag !== 58) {
            break;
          }

          message.zipfile = reader.bytes();
          continue;
        case 8:
          if (tag !== 66) {
            break;
          }

          message.s3Path = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeCode {
    return {
      sourceCode: isSet(object.sourceCode) ? PromptGraphNodeCodeSourceCode.fromJSON(object.sourceCode) : undefined,
      zipfile: isSet(object.zipfile) ? bytesFromBase64(object.zipfile) : undefined,
      s3Path: isSet(object.s3Path) ? String(object.s3Path) : undefined,
    };
  },

  toJSON(message: PromptGraphNodeCode): unknown {
    const obj: any = {};
    if (message.sourceCode !== undefined) {
      obj.sourceCode = PromptGraphNodeCodeSourceCode.toJSON(message.sourceCode);
    }
    if (message.zipfile !== undefined) {
      obj.zipfile = base64FromBytes(message.zipfile);
    }
    if (message.s3Path !== undefined) {
      obj.s3Path = message.s3Path;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeCode>, I>>(base?: I): PromptGraphNodeCode {
    return PromptGraphNodeCode.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeCode>, I>>(object: I): PromptGraphNodeCode {
    const message = createBasePromptGraphNodeCode();
    message.sourceCode = (object.sourceCode !== undefined && object.sourceCode !== null)
      ? PromptGraphNodeCodeSourceCode.fromPartial(object.sourceCode)
      : undefined;
    message.zipfile = object.zipfile ?? undefined;
    message.s3Path = object.s3Path ?? undefined;
    return message;
  },
};

function createBasePromptGraphNodeLoader(): PromptGraphNodeLoader {
  return { zipfileBytes: undefined };
}

export const PromptGraphNodeLoader = {
  encode(message: PromptGraphNodeLoader, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.zipfileBytes !== undefined) {
      writer.uint32(10).bytes(message.zipfileBytes);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeLoader {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeLoader();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.zipfileBytes = reader.bytes();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeLoader {
    return { zipfileBytes: isSet(object.zipfileBytes) ? bytesFromBase64(object.zipfileBytes) : undefined };
  },

  toJSON(message: PromptGraphNodeLoader): unknown {
    const obj: any = {};
    if (message.zipfileBytes !== undefined) {
      obj.zipfileBytes = base64FromBytes(message.zipfileBytes);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeLoader>, I>>(base?: I): PromptGraphNodeLoader {
    return PromptGraphNodeLoader.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeLoader>, I>>(object: I): PromptGraphNodeLoader {
    const message = createBasePromptGraphNodeLoader();
    message.zipfileBytes = object.zipfileBytes ?? undefined;
    return message;
  },
};

function createBasePromptGraphNodeCustom(): PromptGraphNodeCustom {
  return { typeName: "" };
}

export const PromptGraphNodeCustom = {
  encode(message: PromptGraphNodeCustom, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.typeName !== "") {
      writer.uint32(10).string(message.typeName);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeCustom {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeCustom();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.typeName = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeCustom {
    return { typeName: isSet(object.typeName) ? String(object.typeName) : "" };
  },

  toJSON(message: PromptGraphNodeCustom): unknown {
    const obj: any = {};
    if (message.typeName !== "") {
      obj.typeName = message.typeName;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeCustom>, I>>(base?: I): PromptGraphNodeCustom {
    return PromptGraphNodeCustom.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeCustom>, I>>(object: I): PromptGraphNodeCustom {
    const message = createBasePromptGraphNodeCustom();
    message.typeName = object.typeName ?? "";
    return message;
  },
};

function createBasePromptGraphNodeSchedule(): PromptGraphNodeSchedule {
  return { crontab: undefined, naturalLanguage: undefined, everyMs: undefined };
}

export const PromptGraphNodeSchedule = {
  encode(message: PromptGraphNodeSchedule, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.crontab !== undefined) {
      writer.uint32(10).string(message.crontab);
    }
    if (message.naturalLanguage !== undefined) {
      writer.uint32(18).string(message.naturalLanguage);
    }
    if (message.everyMs !== undefined) {
      writer.uint32(26).string(message.everyMs);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeSchedule {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeSchedule();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.crontab = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.naturalLanguage = reader.string();
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.everyMs = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeSchedule {
    return {
      crontab: isSet(object.crontab) ? String(object.crontab) : undefined,
      naturalLanguage: isSet(object.naturalLanguage) ? String(object.naturalLanguage) : undefined,
      everyMs: isSet(object.everyMs) ? String(object.everyMs) : undefined,
    };
  },

  toJSON(message: PromptGraphNodeSchedule): unknown {
    const obj: any = {};
    if (message.crontab !== undefined) {
      obj.crontab = message.crontab;
    }
    if (message.naturalLanguage !== undefined) {
      obj.naturalLanguage = message.naturalLanguage;
    }
    if (message.everyMs !== undefined) {
      obj.everyMs = message.everyMs;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeSchedule>, I>>(base?: I): PromptGraphNodeSchedule {
    return PromptGraphNodeSchedule.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeSchedule>, I>>(object: I): PromptGraphNodeSchedule {
    const message = createBasePromptGraphNodeSchedule();
    message.crontab = object.crontab ?? undefined;
    message.naturalLanguage = object.naturalLanguage ?? undefined;
    message.everyMs = object.everyMs ?? undefined;
    return message;
  },
};

function createBasePromptGraphNodePrompt(): PromptGraphNodePrompt {
  return {
    template: "",
    chatModel: undefined,
    completionModel: undefined,
    temperature: 0,
    topP: 0,
    maxTokens: 0,
    presencePenalty: 0,
    frequencyPenalty: 0,
    stop: [],
  };
}

export const PromptGraphNodePrompt = {
  encode(message: PromptGraphNodePrompt, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.template !== "") {
      writer.uint32(34).string(message.template);
    }
    if (message.chatModel !== undefined) {
      writer.uint32(40).int32(message.chatModel);
    }
    if (message.completionModel !== undefined) {
      writer.uint32(48).int32(message.completionModel);
    }
    if (message.temperature !== 0) {
      writer.uint32(61).float(message.temperature);
    }
    if (message.topP !== 0) {
      writer.uint32(69).float(message.topP);
    }
    if (message.maxTokens !== 0) {
      writer.uint32(72).int32(message.maxTokens);
    }
    if (message.presencePenalty !== 0) {
      writer.uint32(85).float(message.presencePenalty);
    }
    if (message.frequencyPenalty !== 0) {
      writer.uint32(93).float(message.frequencyPenalty);
    }
    for (const v of message.stop) {
      writer.uint32(98).string(v!);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodePrompt {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodePrompt();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 4:
          if (tag !== 34) {
            break;
          }

          message.template = reader.string();
          continue;
        case 5:
          if (tag !== 40) {
            break;
          }

          message.chatModel = reader.int32() as any;
          continue;
        case 6:
          if (tag !== 48) {
            break;
          }

          message.completionModel = reader.int32() as any;
          continue;
        case 7:
          if (tag !== 61) {
            break;
          }

          message.temperature = reader.float();
          continue;
        case 8:
          if (tag !== 69) {
            break;
          }

          message.topP = reader.float();
          continue;
        case 9:
          if (tag !== 72) {
            break;
          }

          message.maxTokens = reader.int32();
          continue;
        case 10:
          if (tag !== 85) {
            break;
          }

          message.presencePenalty = reader.float();
          continue;
        case 11:
          if (tag !== 93) {
            break;
          }

          message.frequencyPenalty = reader.float();
          continue;
        case 12:
          if (tag !== 98) {
            break;
          }

          message.stop.push(reader.string());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodePrompt {
    return {
      template: isSet(object.template) ? String(object.template) : "",
      chatModel: isSet(object.chatModel) ? supportedChatModelFromJSON(object.chatModel) : undefined,
      completionModel: isSet(object.completionModel)
        ? supportedCompletionModelFromJSON(object.completionModel)
        : undefined,
      temperature: isSet(object.temperature) ? Number(object.temperature) : 0,
      topP: isSet(object.topP) ? Number(object.topP) : 0,
      maxTokens: isSet(object.maxTokens) ? Number(object.maxTokens) : 0,
      presencePenalty: isSet(object.presencePenalty) ? Number(object.presencePenalty) : 0,
      frequencyPenalty: isSet(object.frequencyPenalty) ? Number(object.frequencyPenalty) : 0,
      stop: Array.isArray(object?.stop) ? object.stop.map((e: any) => String(e)) : [],
    };
  },

  toJSON(message: PromptGraphNodePrompt): unknown {
    const obj: any = {};
    if (message.template !== "") {
      obj.template = message.template;
    }
    if (message.chatModel !== undefined) {
      obj.chatModel = supportedChatModelToJSON(message.chatModel);
    }
    if (message.completionModel !== undefined) {
      obj.completionModel = supportedCompletionModelToJSON(message.completionModel);
    }
    if (message.temperature !== 0) {
      obj.temperature = message.temperature;
    }
    if (message.topP !== 0) {
      obj.topP = message.topP;
    }
    if (message.maxTokens !== 0) {
      obj.maxTokens = Math.round(message.maxTokens);
    }
    if (message.presencePenalty !== 0) {
      obj.presencePenalty = message.presencePenalty;
    }
    if (message.frequencyPenalty !== 0) {
      obj.frequencyPenalty = message.frequencyPenalty;
    }
    if (message.stop?.length) {
      obj.stop = message.stop;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodePrompt>, I>>(base?: I): PromptGraphNodePrompt {
    return PromptGraphNodePrompt.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodePrompt>, I>>(object: I): PromptGraphNodePrompt {
    const message = createBasePromptGraphNodePrompt();
    message.template = object.template ?? "";
    message.chatModel = object.chatModel ?? undefined;
    message.completionModel = object.completionModel ?? undefined;
    message.temperature = object.temperature ?? 0;
    message.topP = object.topP ?? 0;
    message.maxTokens = object.maxTokens ?? 0;
    message.presencePenalty = object.presencePenalty ?? 0;
    message.frequencyPenalty = object.frequencyPenalty ?? 0;
    message.stop = object.stop?.map((e) => e) || [];
    return message;
  },
};

function createBasePromptGraphNodeMemory(): PromptGraphNodeMemory {
  return { collectionName: "", template: "", model: undefined, db: undefined, action: 0 };
}

export const PromptGraphNodeMemory = {
  encode(message: PromptGraphNodeMemory, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.collectionName !== "") {
      writer.uint32(26).string(message.collectionName);
    }
    if (message.template !== "") {
      writer.uint32(34).string(message.template);
    }
    if (message.model !== undefined) {
      writer.uint32(40).int32(message.model);
    }
    if (message.db !== undefined) {
      writer.uint32(48).int32(message.db);
    }
    if (message.action !== 0) {
      writer.uint32(56).int32(message.action);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeMemory {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeMemory();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 3:
          if (tag !== 26) {
            break;
          }

          message.collectionName = reader.string();
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.template = reader.string();
          continue;
        case 5:
          if (tag !== 40) {
            break;
          }

          message.model = reader.int32() as any;
          continue;
        case 6:
          if (tag !== 48) {
            break;
          }

          message.db = reader.int32() as any;
          continue;
        case 7:
          if (tag !== 56) {
            break;
          }

          message.action = reader.int32() as any;
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeMemory {
    return {
      collectionName: isSet(object.collectionName) ? String(object.collectionName) : "",
      template: isSet(object.template) ? String(object.template) : "",
      model: isSet(object.model) ? supportedEmebddingModelFromJSON(object.model) : undefined,
      db: isSet(object.db) ? supportedVectorDatabaseFromJSON(object.db) : undefined,
      action: isSet(object.action) ? memoryActionFromJSON(object.action) : 0,
    };
  },

  toJSON(message: PromptGraphNodeMemory): unknown {
    const obj: any = {};
    if (message.collectionName !== "") {
      obj.collectionName = message.collectionName;
    }
    if (message.template !== "") {
      obj.template = message.template;
    }
    if (message.model !== undefined) {
      obj.model = supportedEmebddingModelToJSON(message.model);
    }
    if (message.db !== undefined) {
      obj.db = supportedVectorDatabaseToJSON(message.db);
    }
    if (message.action !== 0) {
      obj.action = memoryActionToJSON(message.action);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeMemory>, I>>(base?: I): PromptGraphNodeMemory {
    return PromptGraphNodeMemory.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeMemory>, I>>(object: I): PromptGraphNodeMemory {
    const message = createBasePromptGraphNodeMemory();
    message.collectionName = object.collectionName ?? "";
    message.template = object.template ?? "";
    message.model = object.model ?? undefined;
    message.db = object.db ?? undefined;
    message.action = object.action ?? 0;
    return message;
  },
};

function createBasePromptGraphNodeObservation(): PromptGraphNodeObservation {
  return { integration: "" };
}

export const PromptGraphNodeObservation = {
  encode(message: PromptGraphNodeObservation, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.integration !== "") {
      writer.uint32(34).string(message.integration);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeObservation {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeObservation();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 4:
          if (tag !== 34) {
            break;
          }

          message.integration = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeObservation {
    return { integration: isSet(object.integration) ? String(object.integration) : "" };
  },

  toJSON(message: PromptGraphNodeObservation): unknown {
    const obj: any = {};
    if (message.integration !== "") {
      obj.integration = message.integration;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeObservation>, I>>(base?: I): PromptGraphNodeObservation {
    return PromptGraphNodeObservation.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeObservation>, I>>(object: I): PromptGraphNodeObservation {
    const message = createBasePromptGraphNodeObservation();
    message.integration = object.integration ?? "";
    return message;
  },
};

function createBasePromptGraphNodeComponent(): PromptGraphNodeComponent {
  return { inlineFile: undefined, bytesReference: undefined, s3PathReference: undefined };
}

export const PromptGraphNodeComponent = {
  encode(message: PromptGraphNodeComponent, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.inlineFile !== undefined) {
      File.encode(message.inlineFile, writer.uint32(34).fork()).ldelim();
    }
    if (message.bytesReference !== undefined) {
      writer.uint32(42).bytes(message.bytesReference);
    }
    if (message.s3PathReference !== undefined) {
      writer.uint32(50).string(message.s3PathReference);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeComponent {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeComponent();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 4:
          if (tag !== 34) {
            break;
          }

          message.inlineFile = File.decode(reader, reader.uint32());
          continue;
        case 5:
          if (tag !== 42) {
            break;
          }

          message.bytesReference = reader.bytes();
          continue;
        case 6:
          if (tag !== 50) {
            break;
          }

          message.s3PathReference = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptGraphNodeComponent {
    return {
      inlineFile: isSet(object.inlineFile) ? File.fromJSON(object.inlineFile) : undefined,
      bytesReference: isSet(object.bytesReference) ? bytesFromBase64(object.bytesReference) : undefined,
      s3PathReference: isSet(object.s3PathReference) ? String(object.s3PathReference) : undefined,
    };
  },

  toJSON(message: PromptGraphNodeComponent): unknown {
    const obj: any = {};
    if (message.inlineFile !== undefined) {
      obj.inlineFile = File.toJSON(message.inlineFile);
    }
    if (message.bytesReference !== undefined) {
      obj.bytesReference = base64FromBytes(message.bytesReference);
    }
    if (message.s3PathReference !== undefined) {
      obj.s3PathReference = message.s3PathReference;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeComponent>, I>>(base?: I): PromptGraphNodeComponent {
    return PromptGraphNodeComponent.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeComponent>, I>>(object: I): PromptGraphNodeComponent {
    const message = createBasePromptGraphNodeComponent();
    message.inlineFile = (object.inlineFile !== undefined && object.inlineFile !== null)
      ? File.fromPartial(object.inlineFile)
      : undefined;
    message.bytesReference = object.bytesReference ?? undefined;
    message.s3PathReference = object.s3PathReference ?? undefined;
    return message;
  },
};

function createBasePromptGraphNodeEcho(): PromptGraphNodeEcho {
  return {};
}

export const PromptGraphNodeEcho = {
  encode(_: PromptGraphNodeEcho, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeEcho {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeEcho();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(_: any): PromptGraphNodeEcho {
    return {};
  },

  toJSON(_: PromptGraphNodeEcho): unknown {
    const obj: any = {};
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeEcho>, I>>(base?: I): PromptGraphNodeEcho {
    return PromptGraphNodeEcho.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeEcho>, I>>(_: I): PromptGraphNodeEcho {
    const message = createBasePromptGraphNodeEcho();
    return message;
  },
};

function createBasePromptGraphNodeJoin(): PromptGraphNodeJoin {
  return {};
}

export const PromptGraphNodeJoin = {
  encode(_: PromptGraphNodeJoin, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptGraphNodeJoin {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptGraphNodeJoin();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(_: any): PromptGraphNodeJoin {
    return {};
  },

  toJSON(_: PromptGraphNodeJoin): unknown {
    const obj: any = {};
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptGraphNodeJoin>, I>>(base?: I): PromptGraphNodeJoin {
    return PromptGraphNodeJoin.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptGraphNodeJoin>, I>>(_: I): PromptGraphNodeJoin {
    const message = createBasePromptGraphNodeJoin();
    return message;
  },
};

function createBaseItemCore(): ItemCore {
  return { name: "", queries: [], outputTables: [], output: undefined };
}

export const ItemCore = {
  encode(message: ItemCore, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.name !== "") {
      writer.uint32(10).string(message.name);
    }
    for (const v of message.queries) {
      Query.encode(v!, writer.uint32(18).fork()).ldelim();
    }
    for (const v of message.outputTables) {
      writer.uint32(26).string(v!);
    }
    if (message.output !== undefined) {
      OutputType.encode(message.output, writer.uint32(34).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ItemCore {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseItemCore();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.name = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.queries.push(Query.decode(reader, reader.uint32()));
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.outputTables.push(reader.string());
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.output = OutputType.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ItemCore {
    return {
      name: isSet(object.name) ? String(object.name) : "",
      queries: Array.isArray(object?.queries) ? object.queries.map((e: any) => Query.fromJSON(e)) : [],
      outputTables: Array.isArray(object?.outputTables) ? object.outputTables.map((e: any) => String(e)) : [],
      output: isSet(object.output) ? OutputType.fromJSON(object.output) : undefined,
    };
  },

  toJSON(message: ItemCore): unknown {
    const obj: any = {};
    if (message.name !== "") {
      obj.name = message.name;
    }
    if (message.queries?.length) {
      obj.queries = message.queries.map((e) => Query.toJSON(e));
    }
    if (message.outputTables?.length) {
      obj.outputTables = message.outputTables;
    }
    if (message.output !== undefined) {
      obj.output = OutputType.toJSON(message.output);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ItemCore>, I>>(base?: I): ItemCore {
    return ItemCore.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ItemCore>, I>>(object: I): ItemCore {
    const message = createBaseItemCore();
    message.name = object.name ?? "";
    message.queries = object.queries?.map((e) => Query.fromPartial(e)) || [];
    message.outputTables = object.outputTables?.map((e) => e) || [];
    message.output = (object.output !== undefined && object.output !== null)
      ? OutputType.fromPartial(object.output)
      : undefined;
    return message;
  },
};

function createBaseItem(): Item {
  return {
    core: undefined,
    alias: undefined,
    map: undefined,
    constant: undefined,
    variable: undefined,
    output: undefined,
    nodeCode: undefined,
    nodePrompt: undefined,
    nodeMemory: undefined,
    nodeComponent: undefined,
    nodeObservation: undefined,
    nodeParameter: undefined,
    nodeEcho: undefined,
    nodeLoader: undefined,
    nodeCustom: undefined,
    nodeJoin: undefined,
    nodeSchedule: undefined,
  };
}

export const Item = {
  encode(message: Item, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.core !== undefined) {
      ItemCore.encode(message.core, writer.uint32(10).fork()).ldelim();
    }
    if (message.alias !== undefined) {
      PromptGraphAlias.encode(message.alias, writer.uint32(18).fork()).ldelim();
    }
    if (message.map !== undefined) {
      PromptGraphMap.encode(message.map, writer.uint32(26).fork()).ldelim();
    }
    if (message.constant !== undefined) {
      PromptGraphConstant.encode(message.constant, writer.uint32(34).fork()).ldelim();
    }
    if (message.variable !== undefined) {
      PromptGraphVar.encode(message.variable, writer.uint32(42).fork()).ldelim();
    }
    if (message.output !== undefined) {
      PromptGraphOutputValue.encode(message.output, writer.uint32(50).fork()).ldelim();
    }
    if (message.nodeCode !== undefined) {
      PromptGraphNodeCode.encode(message.nodeCode, writer.uint32(58).fork()).ldelim();
    }
    if (message.nodePrompt !== undefined) {
      PromptGraphNodePrompt.encode(message.nodePrompt, writer.uint32(66).fork()).ldelim();
    }
    if (message.nodeMemory !== undefined) {
      PromptGraphNodeMemory.encode(message.nodeMemory, writer.uint32(74).fork()).ldelim();
    }
    if (message.nodeComponent !== undefined) {
      PromptGraphNodeComponent.encode(message.nodeComponent, writer.uint32(82).fork()).ldelim();
    }
    if (message.nodeObservation !== undefined) {
      PromptGraphNodeObservation.encode(message.nodeObservation, writer.uint32(90).fork()).ldelim();
    }
    if (message.nodeParameter !== undefined) {
      PromptGraphParameterNode.encode(message.nodeParameter, writer.uint32(98).fork()).ldelim();
    }
    if (message.nodeEcho !== undefined) {
      PromptGraphNodeEcho.encode(message.nodeEcho, writer.uint32(106).fork()).ldelim();
    }
    if (message.nodeLoader !== undefined) {
      PromptGraphNodeLoader.encode(message.nodeLoader, writer.uint32(114).fork()).ldelim();
    }
    if (message.nodeCustom !== undefined) {
      PromptGraphNodeCustom.encode(message.nodeCustom, writer.uint32(122).fork()).ldelim();
    }
    if (message.nodeJoin !== undefined) {
      PromptGraphNodeJoin.encode(message.nodeJoin, writer.uint32(130).fork()).ldelim();
    }
    if (message.nodeSchedule !== undefined) {
      PromptGraphNodeSchedule.encode(message.nodeSchedule, writer.uint32(138).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): Item {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseItem();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.core = ItemCore.decode(reader, reader.uint32());
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.alias = PromptGraphAlias.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.map = PromptGraphMap.decode(reader, reader.uint32());
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.constant = PromptGraphConstant.decode(reader, reader.uint32());
          continue;
        case 5:
          if (tag !== 42) {
            break;
          }

          message.variable = PromptGraphVar.decode(reader, reader.uint32());
          continue;
        case 6:
          if (tag !== 50) {
            break;
          }

          message.output = PromptGraphOutputValue.decode(reader, reader.uint32());
          continue;
        case 7:
          if (tag !== 58) {
            break;
          }

          message.nodeCode = PromptGraphNodeCode.decode(reader, reader.uint32());
          continue;
        case 8:
          if (tag !== 66) {
            break;
          }

          message.nodePrompt = PromptGraphNodePrompt.decode(reader, reader.uint32());
          continue;
        case 9:
          if (tag !== 74) {
            break;
          }

          message.nodeMemory = PromptGraphNodeMemory.decode(reader, reader.uint32());
          continue;
        case 10:
          if (tag !== 82) {
            break;
          }

          message.nodeComponent = PromptGraphNodeComponent.decode(reader, reader.uint32());
          continue;
        case 11:
          if (tag !== 90) {
            break;
          }

          message.nodeObservation = PromptGraphNodeObservation.decode(reader, reader.uint32());
          continue;
        case 12:
          if (tag !== 98) {
            break;
          }

          message.nodeParameter = PromptGraphParameterNode.decode(reader, reader.uint32());
          continue;
        case 13:
          if (tag !== 106) {
            break;
          }

          message.nodeEcho = PromptGraphNodeEcho.decode(reader, reader.uint32());
          continue;
        case 14:
          if (tag !== 114) {
            break;
          }

          message.nodeLoader = PromptGraphNodeLoader.decode(reader, reader.uint32());
          continue;
        case 15:
          if (tag !== 122) {
            break;
          }

          message.nodeCustom = PromptGraphNodeCustom.decode(reader, reader.uint32());
          continue;
        case 16:
          if (tag !== 130) {
            break;
          }

          message.nodeJoin = PromptGraphNodeJoin.decode(reader, reader.uint32());
          continue;
        case 17:
          if (tag !== 138) {
            break;
          }

          message.nodeSchedule = PromptGraphNodeSchedule.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): Item {
    return {
      core: isSet(object.core) ? ItemCore.fromJSON(object.core) : undefined,
      alias: isSet(object.alias) ? PromptGraphAlias.fromJSON(object.alias) : undefined,
      map: isSet(object.map) ? PromptGraphMap.fromJSON(object.map) : undefined,
      constant: isSet(object.constant) ? PromptGraphConstant.fromJSON(object.constant) : undefined,
      variable: isSet(object.variable) ? PromptGraphVar.fromJSON(object.variable) : undefined,
      output: isSet(object.output) ? PromptGraphOutputValue.fromJSON(object.output) : undefined,
      nodeCode: isSet(object.nodeCode) ? PromptGraphNodeCode.fromJSON(object.nodeCode) : undefined,
      nodePrompt: isSet(object.nodePrompt) ? PromptGraphNodePrompt.fromJSON(object.nodePrompt) : undefined,
      nodeMemory: isSet(object.nodeMemory) ? PromptGraphNodeMemory.fromJSON(object.nodeMemory) : undefined,
      nodeComponent: isSet(object.nodeComponent) ? PromptGraphNodeComponent.fromJSON(object.nodeComponent) : undefined,
      nodeObservation: isSet(object.nodeObservation)
        ? PromptGraphNodeObservation.fromJSON(object.nodeObservation)
        : undefined,
      nodeParameter: isSet(object.nodeParameter) ? PromptGraphParameterNode.fromJSON(object.nodeParameter) : undefined,
      nodeEcho: isSet(object.nodeEcho) ? PromptGraphNodeEcho.fromJSON(object.nodeEcho) : undefined,
      nodeLoader: isSet(object.nodeLoader) ? PromptGraphNodeLoader.fromJSON(object.nodeLoader) : undefined,
      nodeCustom: isSet(object.nodeCustom) ? PromptGraphNodeCustom.fromJSON(object.nodeCustom) : undefined,
      nodeJoin: isSet(object.nodeJoin) ? PromptGraphNodeJoin.fromJSON(object.nodeJoin) : undefined,
      nodeSchedule: isSet(object.nodeSchedule) ? PromptGraphNodeSchedule.fromJSON(object.nodeSchedule) : undefined,
    };
  },

  toJSON(message: Item): unknown {
    const obj: any = {};
    if (message.core !== undefined) {
      obj.core = ItemCore.toJSON(message.core);
    }
    if (message.alias !== undefined) {
      obj.alias = PromptGraphAlias.toJSON(message.alias);
    }
    if (message.map !== undefined) {
      obj.map = PromptGraphMap.toJSON(message.map);
    }
    if (message.constant !== undefined) {
      obj.constant = PromptGraphConstant.toJSON(message.constant);
    }
    if (message.variable !== undefined) {
      obj.variable = PromptGraphVar.toJSON(message.variable);
    }
    if (message.output !== undefined) {
      obj.output = PromptGraphOutputValue.toJSON(message.output);
    }
    if (message.nodeCode !== undefined) {
      obj.nodeCode = PromptGraphNodeCode.toJSON(message.nodeCode);
    }
    if (message.nodePrompt !== undefined) {
      obj.nodePrompt = PromptGraphNodePrompt.toJSON(message.nodePrompt);
    }
    if (message.nodeMemory !== undefined) {
      obj.nodeMemory = PromptGraphNodeMemory.toJSON(message.nodeMemory);
    }
    if (message.nodeComponent !== undefined) {
      obj.nodeComponent = PromptGraphNodeComponent.toJSON(message.nodeComponent);
    }
    if (message.nodeObservation !== undefined) {
      obj.nodeObservation = PromptGraphNodeObservation.toJSON(message.nodeObservation);
    }
    if (message.nodeParameter !== undefined) {
      obj.nodeParameter = PromptGraphParameterNode.toJSON(message.nodeParameter);
    }
    if (message.nodeEcho !== undefined) {
      obj.nodeEcho = PromptGraphNodeEcho.toJSON(message.nodeEcho);
    }
    if (message.nodeLoader !== undefined) {
      obj.nodeLoader = PromptGraphNodeLoader.toJSON(message.nodeLoader);
    }
    if (message.nodeCustom !== undefined) {
      obj.nodeCustom = PromptGraphNodeCustom.toJSON(message.nodeCustom);
    }
    if (message.nodeJoin !== undefined) {
      obj.nodeJoin = PromptGraphNodeJoin.toJSON(message.nodeJoin);
    }
    if (message.nodeSchedule !== undefined) {
      obj.nodeSchedule = PromptGraphNodeSchedule.toJSON(message.nodeSchedule);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<Item>, I>>(base?: I): Item {
    return Item.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<Item>, I>>(object: I): Item {
    const message = createBaseItem();
    message.core = (object.core !== undefined && object.core !== null) ? ItemCore.fromPartial(object.core) : undefined;
    message.alias = (object.alias !== undefined && object.alias !== null)
      ? PromptGraphAlias.fromPartial(object.alias)
      : undefined;
    message.map = (object.map !== undefined && object.map !== null)
      ? PromptGraphMap.fromPartial(object.map)
      : undefined;
    message.constant = (object.constant !== undefined && object.constant !== null)
      ? PromptGraphConstant.fromPartial(object.constant)
      : undefined;
    message.variable = (object.variable !== undefined && object.variable !== null)
      ? PromptGraphVar.fromPartial(object.variable)
      : undefined;
    message.output = (object.output !== undefined && object.output !== null)
      ? PromptGraphOutputValue.fromPartial(object.output)
      : undefined;
    message.nodeCode = (object.nodeCode !== undefined && object.nodeCode !== null)
      ? PromptGraphNodeCode.fromPartial(object.nodeCode)
      : undefined;
    message.nodePrompt = (object.nodePrompt !== undefined && object.nodePrompt !== null)
      ? PromptGraphNodePrompt.fromPartial(object.nodePrompt)
      : undefined;
    message.nodeMemory = (object.nodeMemory !== undefined && object.nodeMemory !== null)
      ? PromptGraphNodeMemory.fromPartial(object.nodeMemory)
      : undefined;
    message.nodeComponent = (object.nodeComponent !== undefined && object.nodeComponent !== null)
      ? PromptGraphNodeComponent.fromPartial(object.nodeComponent)
      : undefined;
    message.nodeObservation = (object.nodeObservation !== undefined && object.nodeObservation !== null)
      ? PromptGraphNodeObservation.fromPartial(object.nodeObservation)
      : undefined;
    message.nodeParameter = (object.nodeParameter !== undefined && object.nodeParameter !== null)
      ? PromptGraphParameterNode.fromPartial(object.nodeParameter)
      : undefined;
    message.nodeEcho = (object.nodeEcho !== undefined && object.nodeEcho !== null)
      ? PromptGraphNodeEcho.fromPartial(object.nodeEcho)
      : undefined;
    message.nodeLoader = (object.nodeLoader !== undefined && object.nodeLoader !== null)
      ? PromptGraphNodeLoader.fromPartial(object.nodeLoader)
      : undefined;
    message.nodeCustom = (object.nodeCustom !== undefined && object.nodeCustom !== null)
      ? PromptGraphNodeCustom.fromPartial(object.nodeCustom)
      : undefined;
    message.nodeJoin = (object.nodeJoin !== undefined && object.nodeJoin !== null)
      ? PromptGraphNodeJoin.fromPartial(object.nodeJoin)
      : undefined;
    message.nodeSchedule = (object.nodeSchedule !== undefined && object.nodeSchedule !== null)
      ? PromptGraphNodeSchedule.fromPartial(object.nodeSchedule)
      : undefined;
    return message;
  },
};

function createBaseFile(): File {
  return { id: "", nodes: [] };
}

export const File = {
  encode(message: File, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    for (const v of message.nodes) {
      Item.encode(v!, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): File {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseFile();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.nodes.push(Item.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): File {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      nodes: Array.isArray(object?.nodes) ? object.nodes.map((e: any) => Item.fromJSON(e)) : [],
    };
  },

  toJSON(message: File): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.nodes?.length) {
      obj.nodes = message.nodes.map((e) => Item.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<File>, I>>(base?: I): File {
    return File.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<File>, I>>(object: I): File {
    const message = createBaseFile();
    message.id = object.id ?? "";
    message.nodes = object.nodes?.map((e) => Item.fromPartial(e)) || [];
    return message;
  },
};

function createBasePath(): Path {
  return { address: [] };
}

export const Path = {
  encode(message: Path, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.address) {
      writer.uint32(10).string(v!);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): Path {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePath();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.address.push(reader.string());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): Path {
    return { address: Array.isArray(object?.address) ? object.address.map((e: any) => String(e)) : [] };
  },

  toJSON(message: Path): unknown {
    const obj: any = {};
    if (message.address?.length) {
      obj.address = message.address;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<Path>, I>>(base?: I): Path {
    return Path.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<Path>, I>>(object: I): Path {
    const message = createBasePath();
    message.address = object.address?.map((e) => e) || [];
    return message;
  },
};

function createBaseTypeDefinition(): TypeDefinition {
  return {
    primitive: undefined,
    array: undefined,
    object: undefined,
    union: undefined,
    intersection: undefined,
    optional: undefined,
    enum: undefined,
  };
}

export const TypeDefinition = {
  encode(message: TypeDefinition, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.primitive !== undefined) {
      PrimitiveType.encode(message.primitive, writer.uint32(10).fork()).ldelim();
    }
    if (message.array !== undefined) {
      ArrayType.encode(message.array, writer.uint32(18).fork()).ldelim();
    }
    if (message.object !== undefined) {
      ObjectType.encode(message.object, writer.uint32(26).fork()).ldelim();
    }
    if (message.union !== undefined) {
      UnionType.encode(message.union, writer.uint32(34).fork()).ldelim();
    }
    if (message.intersection !== undefined) {
      IntersectionType.encode(message.intersection, writer.uint32(42).fork()).ldelim();
    }
    if (message.optional !== undefined) {
      OptionalType.encode(message.optional, writer.uint32(50).fork()).ldelim();
    }
    if (message.enum !== undefined) {
      EnumType.encode(message.enum, writer.uint32(58).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): TypeDefinition {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseTypeDefinition();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.primitive = PrimitiveType.decode(reader, reader.uint32());
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.array = ArrayType.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.object = ObjectType.decode(reader, reader.uint32());
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.union = UnionType.decode(reader, reader.uint32());
          continue;
        case 5:
          if (tag !== 42) {
            break;
          }

          message.intersection = IntersectionType.decode(reader, reader.uint32());
          continue;
        case 6:
          if (tag !== 50) {
            break;
          }

          message.optional = OptionalType.decode(reader, reader.uint32());
          continue;
        case 7:
          if (tag !== 58) {
            break;
          }

          message.enum = EnumType.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): TypeDefinition {
    return {
      primitive: isSet(object.primitive) ? PrimitiveType.fromJSON(object.primitive) : undefined,
      array: isSet(object.array) ? ArrayType.fromJSON(object.array) : undefined,
      object: isSet(object.object) ? ObjectType.fromJSON(object.object) : undefined,
      union: isSet(object.union) ? UnionType.fromJSON(object.union) : undefined,
      intersection: isSet(object.intersection) ? IntersectionType.fromJSON(object.intersection) : undefined,
      optional: isSet(object.optional) ? OptionalType.fromJSON(object.optional) : undefined,
      enum: isSet(object.enum) ? EnumType.fromJSON(object.enum) : undefined,
    };
  },

  toJSON(message: TypeDefinition): unknown {
    const obj: any = {};
    if (message.primitive !== undefined) {
      obj.primitive = PrimitiveType.toJSON(message.primitive);
    }
    if (message.array !== undefined) {
      obj.array = ArrayType.toJSON(message.array);
    }
    if (message.object !== undefined) {
      obj.object = ObjectType.toJSON(message.object);
    }
    if (message.union !== undefined) {
      obj.union = UnionType.toJSON(message.union);
    }
    if (message.intersection !== undefined) {
      obj.intersection = IntersectionType.toJSON(message.intersection);
    }
    if (message.optional !== undefined) {
      obj.optional = OptionalType.toJSON(message.optional);
    }
    if (message.enum !== undefined) {
      obj.enum = EnumType.toJSON(message.enum);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<TypeDefinition>, I>>(base?: I): TypeDefinition {
    return TypeDefinition.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<TypeDefinition>, I>>(object: I): TypeDefinition {
    const message = createBaseTypeDefinition();
    message.primitive = (object.primitive !== undefined && object.primitive !== null)
      ? PrimitiveType.fromPartial(object.primitive)
      : undefined;
    message.array = (object.array !== undefined && object.array !== null)
      ? ArrayType.fromPartial(object.array)
      : undefined;
    message.object = (object.object !== undefined && object.object !== null)
      ? ObjectType.fromPartial(object.object)
      : undefined;
    message.union = (object.union !== undefined && object.union !== null)
      ? UnionType.fromPartial(object.union)
      : undefined;
    message.intersection = (object.intersection !== undefined && object.intersection !== null)
      ? IntersectionType.fromPartial(object.intersection)
      : undefined;
    message.optional = (object.optional !== undefined && object.optional !== null)
      ? OptionalType.fromPartial(object.optional)
      : undefined;
    message.enum = (object.enum !== undefined && object.enum !== null) ? EnumType.fromPartial(object.enum) : undefined;
    return message;
  },
};

function createBasePrimitiveType(): PrimitiveType {
  return { isString: undefined, isNumber: undefined, isBoolean: undefined, isNull: undefined, isUndefined: undefined };
}

export const PrimitiveType = {
  encode(message: PrimitiveType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.isString !== undefined) {
      writer.uint32(8).bool(message.isString);
    }
    if (message.isNumber !== undefined) {
      writer.uint32(16).bool(message.isNumber);
    }
    if (message.isBoolean !== undefined) {
      writer.uint32(24).bool(message.isBoolean);
    }
    if (message.isNull !== undefined) {
      writer.uint32(32).bool(message.isNull);
    }
    if (message.isUndefined !== undefined) {
      writer.uint32(40).bool(message.isUndefined);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PrimitiveType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePrimitiveType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 8) {
            break;
          }

          message.isString = reader.bool();
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.isNumber = reader.bool();
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.isBoolean = reader.bool();
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.isNull = reader.bool();
          continue;
        case 5:
          if (tag !== 40) {
            break;
          }

          message.isUndefined = reader.bool();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PrimitiveType {
    return {
      isString: isSet(object.isString) ? Boolean(object.isString) : undefined,
      isNumber: isSet(object.isNumber) ? Boolean(object.isNumber) : undefined,
      isBoolean: isSet(object.isBoolean) ? Boolean(object.isBoolean) : undefined,
      isNull: isSet(object.isNull) ? Boolean(object.isNull) : undefined,
      isUndefined: isSet(object.isUndefined) ? Boolean(object.isUndefined) : undefined,
    };
  },

  toJSON(message: PrimitiveType): unknown {
    const obj: any = {};
    if (message.isString !== undefined) {
      obj.isString = message.isString;
    }
    if (message.isNumber !== undefined) {
      obj.isNumber = message.isNumber;
    }
    if (message.isBoolean !== undefined) {
      obj.isBoolean = message.isBoolean;
    }
    if (message.isNull !== undefined) {
      obj.isNull = message.isNull;
    }
    if (message.isUndefined !== undefined) {
      obj.isUndefined = message.isUndefined;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PrimitiveType>, I>>(base?: I): PrimitiveType {
    return PrimitiveType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PrimitiveType>, I>>(object: I): PrimitiveType {
    const message = createBasePrimitiveType();
    message.isString = object.isString ?? undefined;
    message.isNumber = object.isNumber ?? undefined;
    message.isBoolean = object.isBoolean ?? undefined;
    message.isNull = object.isNull ?? undefined;
    message.isUndefined = object.isUndefined ?? undefined;
    return message;
  },
};

function createBaseArrayType(): ArrayType {
  return { type: undefined };
}

export const ArrayType = {
  encode(message: ArrayType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.type !== undefined) {
      TypeDefinition.encode(message.type, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ArrayType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseArrayType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.type = TypeDefinition.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ArrayType {
    return { type: isSet(object.type) ? TypeDefinition.fromJSON(object.type) : undefined };
  },

  toJSON(message: ArrayType): unknown {
    const obj: any = {};
    if (message.type !== undefined) {
      obj.type = TypeDefinition.toJSON(message.type);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ArrayType>, I>>(base?: I): ArrayType {
    return ArrayType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ArrayType>, I>>(object: I): ArrayType {
    const message = createBaseArrayType();
    message.type = (object.type !== undefined && object.type !== null)
      ? TypeDefinition.fromPartial(object.type)
      : undefined;
    return message;
  },
};

function createBaseObjectType(): ObjectType {
  return { fields: {} };
}

export const ObjectType = {
  encode(message: ObjectType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    Object.entries(message.fields).forEach(([key, value]) => {
      ObjectType_FieldsEntry.encode({ key: key as any, value }, writer.uint32(10).fork()).ldelim();
    });
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ObjectType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseObjectType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          const entry1 = ObjectType_FieldsEntry.decode(reader, reader.uint32());
          if (entry1.value !== undefined) {
            message.fields[entry1.key] = entry1.value;
          }
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ObjectType {
    return {
      fields: isObject(object.fields)
        ? Object.entries(object.fields).reduce<{ [key: string]: TypeDefinition }>((acc, [key, value]) => {
          acc[key] = TypeDefinition.fromJSON(value);
          return acc;
        }, {})
        : {},
    };
  },

  toJSON(message: ObjectType): unknown {
    const obj: any = {};
    if (message.fields) {
      const entries = Object.entries(message.fields);
      if (entries.length > 0) {
        obj.fields = {};
        entries.forEach(([k, v]) => {
          obj.fields[k] = TypeDefinition.toJSON(v);
        });
      }
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ObjectType>, I>>(base?: I): ObjectType {
    return ObjectType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ObjectType>, I>>(object: I): ObjectType {
    const message = createBaseObjectType();
    message.fields = Object.entries(object.fields ?? {}).reduce<{ [key: string]: TypeDefinition }>(
      (acc, [key, value]) => {
        if (value !== undefined) {
          acc[key] = TypeDefinition.fromPartial(value);
        }
        return acc;
      },
      {},
    );
    return message;
  },
};

function createBaseObjectType_FieldsEntry(): ObjectType_FieldsEntry {
  return { key: "", value: undefined };
}

export const ObjectType_FieldsEntry = {
  encode(message: ObjectType_FieldsEntry, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.key !== "") {
      writer.uint32(10).string(message.key);
    }
    if (message.value !== undefined) {
      TypeDefinition.encode(message.value, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ObjectType_FieldsEntry {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseObjectType_FieldsEntry();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.key = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.value = TypeDefinition.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ObjectType_FieldsEntry {
    return {
      key: isSet(object.key) ? String(object.key) : "",
      value: isSet(object.value) ? TypeDefinition.fromJSON(object.value) : undefined,
    };
  },

  toJSON(message: ObjectType_FieldsEntry): unknown {
    const obj: any = {};
    if (message.key !== "") {
      obj.key = message.key;
    }
    if (message.value !== undefined) {
      obj.value = TypeDefinition.toJSON(message.value);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ObjectType_FieldsEntry>, I>>(base?: I): ObjectType_FieldsEntry {
    return ObjectType_FieldsEntry.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ObjectType_FieldsEntry>, I>>(object: I): ObjectType_FieldsEntry {
    const message = createBaseObjectType_FieldsEntry();
    message.key = object.key ?? "";
    message.value = (object.value !== undefined && object.value !== null)
      ? TypeDefinition.fromPartial(object.value)
      : undefined;
    return message;
  },
};

function createBaseUnionType(): UnionType {
  return { types: [] };
}

export const UnionType = {
  encode(message: UnionType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.types) {
      TypeDefinition.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): UnionType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseUnionType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.types.push(TypeDefinition.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): UnionType {
    return { types: Array.isArray(object?.types) ? object.types.map((e: any) => TypeDefinition.fromJSON(e)) : [] };
  },

  toJSON(message: UnionType): unknown {
    const obj: any = {};
    if (message.types?.length) {
      obj.types = message.types.map((e) => TypeDefinition.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<UnionType>, I>>(base?: I): UnionType {
    return UnionType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<UnionType>, I>>(object: I): UnionType {
    const message = createBaseUnionType();
    message.types = object.types?.map((e) => TypeDefinition.fromPartial(e)) || [];
    return message;
  },
};

function createBaseIntersectionType(): IntersectionType {
  return { types: [] };
}

export const IntersectionType = {
  encode(message: IntersectionType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.types) {
      TypeDefinition.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): IntersectionType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseIntersectionType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.types.push(TypeDefinition.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): IntersectionType {
    return { types: Array.isArray(object?.types) ? object.types.map((e: any) => TypeDefinition.fromJSON(e)) : [] };
  },

  toJSON(message: IntersectionType): unknown {
    const obj: any = {};
    if (message.types?.length) {
      obj.types = message.types.map((e) => TypeDefinition.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<IntersectionType>, I>>(base?: I): IntersectionType {
    return IntersectionType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<IntersectionType>, I>>(object: I): IntersectionType {
    const message = createBaseIntersectionType();
    message.types = object.types?.map((e) => TypeDefinition.fromPartial(e)) || [];
    return message;
  },
};

function createBaseOptionalType(): OptionalType {
  return { type: undefined };
}

export const OptionalType = {
  encode(message: OptionalType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.type !== undefined) {
      TypeDefinition.encode(message.type, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): OptionalType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseOptionalType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.type = TypeDefinition.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): OptionalType {
    return { type: isSet(object.type) ? TypeDefinition.fromJSON(object.type) : undefined };
  },

  toJSON(message: OptionalType): unknown {
    const obj: any = {};
    if (message.type !== undefined) {
      obj.type = TypeDefinition.toJSON(message.type);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<OptionalType>, I>>(base?: I): OptionalType {
    return OptionalType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<OptionalType>, I>>(object: I): OptionalType {
    const message = createBaseOptionalType();
    message.type = (object.type !== undefined && object.type !== null)
      ? TypeDefinition.fromPartial(object.type)
      : undefined;
    return message;
  },
};

function createBaseEnumType(): EnumType {
  return { values: {} };
}

export const EnumType = {
  encode(message: EnumType, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    Object.entries(message.values).forEach(([key, value]) => {
      EnumType_ValuesEntry.encode({ key: key as any, value }, writer.uint32(10).fork()).ldelim();
    });
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): EnumType {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseEnumType();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          const entry1 = EnumType_ValuesEntry.decode(reader, reader.uint32());
          if (entry1.value !== undefined) {
            message.values[entry1.key] = entry1.value;
          }
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): EnumType {
    return {
      values: isObject(object.values)
        ? Object.entries(object.values).reduce<{ [key: string]: string }>((acc, [key, value]) => {
          acc[key] = String(value);
          return acc;
        }, {})
        : {},
    };
  },

  toJSON(message: EnumType): unknown {
    const obj: any = {};
    if (message.values) {
      const entries = Object.entries(message.values);
      if (entries.length > 0) {
        obj.values = {};
        entries.forEach(([k, v]) => {
          obj.values[k] = v;
        });
      }
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<EnumType>, I>>(base?: I): EnumType {
    return EnumType.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<EnumType>, I>>(object: I): EnumType {
    const message = createBaseEnumType();
    message.values = Object.entries(object.values ?? {}).reduce<{ [key: string]: string }>((acc, [key, value]) => {
      if (value !== undefined) {
        acc[key] = String(value);
      }
      return acc;
    }, {});
    return message;
  },
};

function createBaseEnumType_ValuesEntry(): EnumType_ValuesEntry {
  return { key: "", value: "" };
}

export const EnumType_ValuesEntry = {
  encode(message: EnumType_ValuesEntry, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.key !== "") {
      writer.uint32(10).string(message.key);
    }
    if (message.value !== "") {
      writer.uint32(18).string(message.value);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): EnumType_ValuesEntry {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseEnumType_ValuesEntry();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.key = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.value = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): EnumType_ValuesEntry {
    return { key: isSet(object.key) ? String(object.key) : "", value: isSet(object.value) ? String(object.value) : "" };
  },

  toJSON(message: EnumType_ValuesEntry): unknown {
    const obj: any = {};
    if (message.key !== "") {
      obj.key = message.key;
    }
    if (message.value !== "") {
      obj.value = message.value;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<EnumType_ValuesEntry>, I>>(base?: I): EnumType_ValuesEntry {
    return EnumType_ValuesEntry.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<EnumType_ValuesEntry>, I>>(object: I): EnumType_ValuesEntry {
    const message = createBaseEnumType_ValuesEntry();
    message.key = object.key ?? "";
    message.value = object.value ?? "";
    return message;
  },
};

function createBaseSerializedValueArray(): SerializedValueArray {
  return { values: [] };
}

export const SerializedValueArray = {
  encode(message: SerializedValueArray, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.values) {
      SerializedValue.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): SerializedValueArray {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseSerializedValueArray();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.values.push(SerializedValue.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): SerializedValueArray {
    return { values: Array.isArray(object?.values) ? object.values.map((e: any) => SerializedValue.fromJSON(e)) : [] };
  },

  toJSON(message: SerializedValueArray): unknown {
    const obj: any = {};
    if (message.values?.length) {
      obj.values = message.values.map((e) => SerializedValue.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<SerializedValueArray>, I>>(base?: I): SerializedValueArray {
    return SerializedValueArray.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<SerializedValueArray>, I>>(object: I): SerializedValueArray {
    const message = createBaseSerializedValueArray();
    message.values = object.values?.map((e) => SerializedValue.fromPartial(e)) || [];
    return message;
  },
};

function createBaseSerializedValueObject(): SerializedValueObject {
  return { values: {} };
}

export const SerializedValueObject = {
  encode(message: SerializedValueObject, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    Object.entries(message.values).forEach(([key, value]) => {
      SerializedValueObject_ValuesEntry.encode({ key: key as any, value }, writer.uint32(10).fork()).ldelim();
    });
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): SerializedValueObject {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseSerializedValueObject();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          const entry1 = SerializedValueObject_ValuesEntry.decode(reader, reader.uint32());
          if (entry1.value !== undefined) {
            message.values[entry1.key] = entry1.value;
          }
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): SerializedValueObject {
    return {
      values: isObject(object.values)
        ? Object.entries(object.values).reduce<{ [key: string]: SerializedValue }>((acc, [key, value]) => {
          acc[key] = SerializedValue.fromJSON(value);
          return acc;
        }, {})
        : {},
    };
  },

  toJSON(message: SerializedValueObject): unknown {
    const obj: any = {};
    if (message.values) {
      const entries = Object.entries(message.values);
      if (entries.length > 0) {
        obj.values = {};
        entries.forEach(([k, v]) => {
          obj.values[k] = SerializedValue.toJSON(v);
        });
      }
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<SerializedValueObject>, I>>(base?: I): SerializedValueObject {
    return SerializedValueObject.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<SerializedValueObject>, I>>(object: I): SerializedValueObject {
    const message = createBaseSerializedValueObject();
    message.values = Object.entries(object.values ?? {}).reduce<{ [key: string]: SerializedValue }>(
      (acc, [key, value]) => {
        if (value !== undefined) {
          acc[key] = SerializedValue.fromPartial(value);
        }
        return acc;
      },
      {},
    );
    return message;
  },
};

function createBaseSerializedValueObject_ValuesEntry(): SerializedValueObject_ValuesEntry {
  return { key: "", value: undefined };
}

export const SerializedValueObject_ValuesEntry = {
  encode(message: SerializedValueObject_ValuesEntry, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.key !== "") {
      writer.uint32(10).string(message.key);
    }
    if (message.value !== undefined) {
      SerializedValue.encode(message.value, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): SerializedValueObject_ValuesEntry {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseSerializedValueObject_ValuesEntry();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.key = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.value = SerializedValue.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): SerializedValueObject_ValuesEntry {
    return {
      key: isSet(object.key) ? String(object.key) : "",
      value: isSet(object.value) ? SerializedValue.fromJSON(object.value) : undefined,
    };
  },

  toJSON(message: SerializedValueObject_ValuesEntry): unknown {
    const obj: any = {};
    if (message.key !== "") {
      obj.key = message.key;
    }
    if (message.value !== undefined) {
      obj.value = SerializedValue.toJSON(message.value);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<SerializedValueObject_ValuesEntry>, I>>(
    base?: I,
  ): SerializedValueObject_ValuesEntry {
    return SerializedValueObject_ValuesEntry.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<SerializedValueObject_ValuesEntry>, I>>(
    object: I,
  ): SerializedValueObject_ValuesEntry {
    const message = createBaseSerializedValueObject_ValuesEntry();
    message.key = object.key ?? "";
    message.value = (object.value !== undefined && object.value !== null)
      ? SerializedValue.fromPartial(object.value)
      : undefined;
    return message;
  },
};

function createBaseSerializedValue(): SerializedValue {
  return {
    float: undefined,
    number: undefined,
    string: undefined,
    boolean: undefined,
    array: undefined,
    object: undefined,
  };
}

export const SerializedValue = {
  encode(message: SerializedValue, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.float !== undefined) {
      writer.uint32(21).float(message.float);
    }
    if (message.number !== undefined) {
      writer.uint32(24).int32(message.number);
    }
    if (message.string !== undefined) {
      writer.uint32(34).string(message.string);
    }
    if (message.boolean !== undefined) {
      writer.uint32(40).bool(message.boolean);
    }
    if (message.array !== undefined) {
      SerializedValueArray.encode(message.array, writer.uint32(50).fork()).ldelim();
    }
    if (message.object !== undefined) {
      SerializedValueObject.encode(message.object, writer.uint32(58).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): SerializedValue {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseSerializedValue();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 2:
          if (tag !== 21) {
            break;
          }

          message.float = reader.float();
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.number = reader.int32();
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.string = reader.string();
          continue;
        case 5:
          if (tag !== 40) {
            break;
          }

          message.boolean = reader.bool();
          continue;
        case 6:
          if (tag !== 50) {
            break;
          }

          message.array = SerializedValueArray.decode(reader, reader.uint32());
          continue;
        case 7:
          if (tag !== 58) {
            break;
          }

          message.object = SerializedValueObject.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): SerializedValue {
    return {
      float: isSet(object.float) ? Number(object.float) : undefined,
      number: isSet(object.number) ? Number(object.number) : undefined,
      string: isSet(object.string) ? String(object.string) : undefined,
      boolean: isSet(object.boolean) ? Boolean(object.boolean) : undefined,
      array: isSet(object.array) ? SerializedValueArray.fromJSON(object.array) : undefined,
      object: isSet(object.object) ? SerializedValueObject.fromJSON(object.object) : undefined,
    };
  },

  toJSON(message: SerializedValue): unknown {
    const obj: any = {};
    if (message.float !== undefined) {
      obj.float = message.float;
    }
    if (message.number !== undefined) {
      obj.number = Math.round(message.number);
    }
    if (message.string !== undefined) {
      obj.string = message.string;
    }
    if (message.boolean !== undefined) {
      obj.boolean = message.boolean;
    }
    if (message.array !== undefined) {
      obj.array = SerializedValueArray.toJSON(message.array);
    }
    if (message.object !== undefined) {
      obj.object = SerializedValueObject.toJSON(message.object);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<SerializedValue>, I>>(base?: I): SerializedValue {
    return SerializedValue.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<SerializedValue>, I>>(object: I): SerializedValue {
    const message = createBaseSerializedValue();
    message.float = object.float ?? undefined;
    message.number = object.number ?? undefined;
    message.string = object.string ?? undefined;
    message.boolean = object.boolean ?? undefined;
    message.array = (object.array !== undefined && object.array !== null)
      ? SerializedValueArray.fromPartial(object.array)
      : undefined;
    message.object = (object.object !== undefined && object.object !== null)
      ? SerializedValueObject.fromPartial(object.object)
      : undefined;
    return message;
  },
};

function createBaseChangeValue(): ChangeValue {
  return { path: undefined, value: undefined, branch: 0 };
}

export const ChangeValue = {
  encode(message: ChangeValue, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.path !== undefined) {
      Path.encode(message.path, writer.uint32(10).fork()).ldelim();
    }
    if (message.value !== undefined) {
      SerializedValue.encode(message.value, writer.uint32(18).fork()).ldelim();
    }
    if (message.branch !== 0) {
      writer.uint32(24).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ChangeValue {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseChangeValue();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.path = Path.decode(reader, reader.uint32());
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.value = SerializedValue.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ChangeValue {
    return {
      path: isSet(object.path) ? Path.fromJSON(object.path) : undefined,
      value: isSet(object.value) ? SerializedValue.fromJSON(object.value) : undefined,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: ChangeValue): unknown {
    const obj: any = {};
    if (message.path !== undefined) {
      obj.path = Path.toJSON(message.path);
    }
    if (message.value !== undefined) {
      obj.value = SerializedValue.toJSON(message.value);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ChangeValue>, I>>(base?: I): ChangeValue {
    return ChangeValue.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ChangeValue>, I>>(object: I): ChangeValue {
    const message = createBaseChangeValue();
    message.path = (object.path !== undefined && object.path !== null) ? Path.fromPartial(object.path) : undefined;
    message.value = (object.value !== undefined && object.value !== null)
      ? SerializedValue.fromPartial(object.value)
      : undefined;
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseWrappedChangeValue(): WrappedChangeValue {
  return { monotonicCounter: 0, changeValue: undefined };
}

export const WrappedChangeValue = {
  encode(message: WrappedChangeValue, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.monotonicCounter !== 0) {
      writer.uint32(24).uint64(message.monotonicCounter);
    }
    if (message.changeValue !== undefined) {
      ChangeValue.encode(message.changeValue, writer.uint32(34).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): WrappedChangeValue {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseWrappedChangeValue();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 3:
          if (tag !== 24) {
            break;
          }

          message.monotonicCounter = longToNumber(reader.uint64() as Long);
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.changeValue = ChangeValue.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): WrappedChangeValue {
    return {
      monotonicCounter: isSet(object.monotonicCounter) ? Number(object.monotonicCounter) : 0,
      changeValue: isSet(object.changeValue) ? ChangeValue.fromJSON(object.changeValue) : undefined,
    };
  },

  toJSON(message: WrappedChangeValue): unknown {
    const obj: any = {};
    if (message.monotonicCounter !== 0) {
      obj.monotonicCounter = Math.round(message.monotonicCounter);
    }
    if (message.changeValue !== undefined) {
      obj.changeValue = ChangeValue.toJSON(message.changeValue);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<WrappedChangeValue>, I>>(base?: I): WrappedChangeValue {
    return WrappedChangeValue.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<WrappedChangeValue>, I>>(object: I): WrappedChangeValue {
    const message = createBaseWrappedChangeValue();
    message.monotonicCounter = object.monotonicCounter ?? 0;
    message.changeValue = (object.changeValue !== undefined && object.changeValue !== null)
      ? ChangeValue.fromPartial(object.changeValue)
      : undefined;
    return message;
  },
};

function createBaseNodeWillExecute(): NodeWillExecute {
  return { sourceNode: "", changeValuesUsedInExecution: [], matchedQueryIndex: 0 };
}

export const NodeWillExecute = {
  encode(message: NodeWillExecute, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.sourceNode !== "") {
      writer.uint32(10).string(message.sourceNode);
    }
    for (const v of message.changeValuesUsedInExecution) {
      WrappedChangeValue.encode(v!, writer.uint32(18).fork()).ldelim();
    }
    if (message.matchedQueryIndex !== 0) {
      writer.uint32(24).uint64(message.matchedQueryIndex);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): NodeWillExecute {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseNodeWillExecute();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.sourceNode = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.changeValuesUsedInExecution.push(WrappedChangeValue.decode(reader, reader.uint32()));
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.matchedQueryIndex = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): NodeWillExecute {
    return {
      sourceNode: isSet(object.sourceNode) ? String(object.sourceNode) : "",
      changeValuesUsedInExecution: Array.isArray(object?.changeValuesUsedInExecution)
        ? object.changeValuesUsedInExecution.map((e: any) => WrappedChangeValue.fromJSON(e))
        : [],
      matchedQueryIndex: isSet(object.matchedQueryIndex) ? Number(object.matchedQueryIndex) : 0,
    };
  },

  toJSON(message: NodeWillExecute): unknown {
    const obj: any = {};
    if (message.sourceNode !== "") {
      obj.sourceNode = message.sourceNode;
    }
    if (message.changeValuesUsedInExecution?.length) {
      obj.changeValuesUsedInExecution = message.changeValuesUsedInExecution.map((e) => WrappedChangeValue.toJSON(e));
    }
    if (message.matchedQueryIndex !== 0) {
      obj.matchedQueryIndex = Math.round(message.matchedQueryIndex);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<NodeWillExecute>, I>>(base?: I): NodeWillExecute {
    return NodeWillExecute.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<NodeWillExecute>, I>>(object: I): NodeWillExecute {
    const message = createBaseNodeWillExecute();
    message.sourceNode = object.sourceNode ?? "";
    message.changeValuesUsedInExecution =
      object.changeValuesUsedInExecution?.map((e) => WrappedChangeValue.fromPartial(e)) || [];
    message.matchedQueryIndex = object.matchedQueryIndex ?? 0;
    return message;
  },
};

function createBaseDispatchResult(): DispatchResult {
  return { operations: [] };
}

export const DispatchResult = {
  encode(message: DispatchResult, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.operations) {
      NodeWillExecute.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): DispatchResult {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseDispatchResult();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.operations.push(NodeWillExecute.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): DispatchResult {
    return {
      operations: Array.isArray(object?.operations)
        ? object.operations.map((e: any) => NodeWillExecute.fromJSON(e))
        : [],
    };
  },

  toJSON(message: DispatchResult): unknown {
    const obj: any = {};
    if (message.operations?.length) {
      obj.operations = message.operations.map((e) => NodeWillExecute.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<DispatchResult>, I>>(base?: I): DispatchResult {
    return DispatchResult.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<DispatchResult>, I>>(object: I): DispatchResult {
    const message = createBaseDispatchResult();
    message.operations = object.operations?.map((e) => NodeWillExecute.fromPartial(e)) || [];
    return message;
  },
};

function createBaseNodeWillExecuteOnBranch(): NodeWillExecuteOnBranch {
  return { branch: 0, counter: 0, customNodeTypeName: undefined, node: undefined };
}

export const NodeWillExecuteOnBranch = {
  encode(message: NodeWillExecuteOnBranch, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.branch !== 0) {
      writer.uint32(8).uint64(message.branch);
    }
    if (message.counter !== 0) {
      writer.uint32(16).uint64(message.counter);
    }
    if (message.customNodeTypeName !== undefined) {
      writer.uint32(26).string(message.customNodeTypeName);
    }
    if (message.node !== undefined) {
      NodeWillExecute.encode(message.node, writer.uint32(34).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): NodeWillExecuteOnBranch {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseNodeWillExecuteOnBranch();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 8) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.counter = longToNumber(reader.uint64() as Long);
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.customNodeTypeName = reader.string();
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.node = NodeWillExecute.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): NodeWillExecuteOnBranch {
    return {
      branch: isSet(object.branch) ? Number(object.branch) : 0,
      counter: isSet(object.counter) ? Number(object.counter) : 0,
      customNodeTypeName: isSet(object.customNodeTypeName) ? String(object.customNodeTypeName) : undefined,
      node: isSet(object.node) ? NodeWillExecute.fromJSON(object.node) : undefined,
    };
  },

  toJSON(message: NodeWillExecuteOnBranch): unknown {
    const obj: any = {};
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    if (message.counter !== 0) {
      obj.counter = Math.round(message.counter);
    }
    if (message.customNodeTypeName !== undefined) {
      obj.customNodeTypeName = message.customNodeTypeName;
    }
    if (message.node !== undefined) {
      obj.node = NodeWillExecute.toJSON(message.node);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<NodeWillExecuteOnBranch>, I>>(base?: I): NodeWillExecuteOnBranch {
    return NodeWillExecuteOnBranch.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<NodeWillExecuteOnBranch>, I>>(object: I): NodeWillExecuteOnBranch {
    const message = createBaseNodeWillExecuteOnBranch();
    message.branch = object.branch ?? 0;
    message.counter = object.counter ?? 0;
    message.customNodeTypeName = object.customNodeTypeName ?? undefined;
    message.node = (object.node !== undefined && object.node !== null)
      ? NodeWillExecute.fromPartial(object.node)
      : undefined;
    return message;
  },
};

function createBaseChangeValueWithCounter(): ChangeValueWithCounter {
  return { filledValues: [], parentMonotonicCounters: [], monotonicCounter: 0, branch: 0, sourceNode: "" };
}

export const ChangeValueWithCounter = {
  encode(message: ChangeValueWithCounter, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.filledValues) {
      ChangeValue.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    writer.uint32(18).fork();
    for (const v of message.parentMonotonicCounters) {
      writer.uint64(v);
    }
    writer.ldelim();
    if (message.monotonicCounter !== 0) {
      writer.uint32(24).uint64(message.monotonicCounter);
    }
    if (message.branch !== 0) {
      writer.uint32(32).uint64(message.branch);
    }
    if (message.sourceNode !== "") {
      writer.uint32(42).string(message.sourceNode);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ChangeValueWithCounter {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseChangeValueWithCounter();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.filledValues.push(ChangeValue.decode(reader, reader.uint32()));
          continue;
        case 2:
          if (tag === 16) {
            message.parentMonotonicCounters.push(longToNumber(reader.uint64() as Long));

            continue;
          }

          if (tag === 18) {
            const end2 = reader.uint32() + reader.pos;
            while (reader.pos < end2) {
              message.parentMonotonicCounters.push(longToNumber(reader.uint64() as Long));
            }

            continue;
          }

          break;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.monotonicCounter = longToNumber(reader.uint64() as Long);
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
        case 5:
          if (tag !== 42) {
            break;
          }

          message.sourceNode = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ChangeValueWithCounter {
    return {
      filledValues: Array.isArray(object?.filledValues)
        ? object.filledValues.map((e: any) => ChangeValue.fromJSON(e))
        : [],
      parentMonotonicCounters: Array.isArray(object?.parentMonotonicCounters)
        ? object.parentMonotonicCounters.map((e: any) => Number(e))
        : [],
      monotonicCounter: isSet(object.monotonicCounter) ? Number(object.monotonicCounter) : 0,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
      sourceNode: isSet(object.sourceNode) ? String(object.sourceNode) : "",
    };
  },

  toJSON(message: ChangeValueWithCounter): unknown {
    const obj: any = {};
    if (message.filledValues?.length) {
      obj.filledValues = message.filledValues.map((e) => ChangeValue.toJSON(e));
    }
    if (message.parentMonotonicCounters?.length) {
      obj.parentMonotonicCounters = message.parentMonotonicCounters.map((e) => Math.round(e));
    }
    if (message.monotonicCounter !== 0) {
      obj.monotonicCounter = Math.round(message.monotonicCounter);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    if (message.sourceNode !== "") {
      obj.sourceNode = message.sourceNode;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ChangeValueWithCounter>, I>>(base?: I): ChangeValueWithCounter {
    return ChangeValueWithCounter.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ChangeValueWithCounter>, I>>(object: I): ChangeValueWithCounter {
    const message = createBaseChangeValueWithCounter();
    message.filledValues = object.filledValues?.map((e) => ChangeValue.fromPartial(e)) || [];
    message.parentMonotonicCounters = object.parentMonotonicCounters?.map((e) => e) || [];
    message.monotonicCounter = object.monotonicCounter ?? 0;
    message.branch = object.branch ?? 0;
    message.sourceNode = object.sourceNode ?? "";
    return message;
  },
};

function createBaseCounterWithPath(): CounterWithPath {
  return { monotonicCounter: 0, path: undefined };
}

export const CounterWithPath = {
  encode(message: CounterWithPath, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.monotonicCounter !== 0) {
      writer.uint32(8).uint64(message.monotonicCounter);
    }
    if (message.path !== undefined) {
      Path.encode(message.path, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): CounterWithPath {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseCounterWithPath();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 8) {
            break;
          }

          message.monotonicCounter = longToNumber(reader.uint64() as Long);
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.path = Path.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): CounterWithPath {
    return {
      monotonicCounter: isSet(object.monotonicCounter) ? Number(object.monotonicCounter) : 0,
      path: isSet(object.path) ? Path.fromJSON(object.path) : undefined,
    };
  },

  toJSON(message: CounterWithPath): unknown {
    const obj: any = {};
    if (message.monotonicCounter !== 0) {
      obj.monotonicCounter = Math.round(message.monotonicCounter);
    }
    if (message.path !== undefined) {
      obj.path = Path.toJSON(message.path);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<CounterWithPath>, I>>(base?: I): CounterWithPath {
    return CounterWithPath.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<CounterWithPath>, I>>(object: I): CounterWithPath {
    const message = createBaseCounterWithPath();
    message.monotonicCounter = object.monotonicCounter ?? 0;
    message.path = (object.path !== undefined && object.path !== null) ? Path.fromPartial(object.path) : undefined;
    return message;
  },
};

function createBaseInputProposal(): InputProposal {
  return { name: "", output: undefined, counter: 0, branch: 0 };
}

export const InputProposal = {
  encode(message: InputProposal, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.name !== "") {
      writer.uint32(10).string(message.name);
    }
    if (message.output !== undefined) {
      OutputType.encode(message.output, writer.uint32(18).fork()).ldelim();
    }
    if (message.counter !== 0) {
      writer.uint32(24).uint64(message.counter);
    }
    if (message.branch !== 0) {
      writer.uint32(32).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): InputProposal {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseInputProposal();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.name = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.output = OutputType.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.counter = longToNumber(reader.uint64() as Long);
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): InputProposal {
    return {
      name: isSet(object.name) ? String(object.name) : "",
      output: isSet(object.output) ? OutputType.fromJSON(object.output) : undefined,
      counter: isSet(object.counter) ? Number(object.counter) : 0,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: InputProposal): unknown {
    const obj: any = {};
    if (message.name !== "") {
      obj.name = message.name;
    }
    if (message.output !== undefined) {
      obj.output = OutputType.toJSON(message.output);
    }
    if (message.counter !== 0) {
      obj.counter = Math.round(message.counter);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<InputProposal>, I>>(base?: I): InputProposal {
    return InputProposal.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<InputProposal>, I>>(object: I): InputProposal {
    const message = createBaseInputProposal();
    message.name = object.name ?? "";
    message.output = (object.output !== undefined && object.output !== null)
      ? OutputType.fromPartial(object.output)
      : undefined;
    message.counter = object.counter ?? 0;
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseRequestInputProposalResponse(): RequestInputProposalResponse {
  return { id: "", proposalCounter: 0, changes: [], branch: 0 };
}

export const RequestInputProposalResponse = {
  encode(message: RequestInputProposalResponse, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.proposalCounter !== 0) {
      writer.uint32(16).uint64(message.proposalCounter);
    }
    for (const v of message.changes) {
      ChangeValue.encode(v!, writer.uint32(26).fork()).ldelim();
    }
    if (message.branch !== 0) {
      writer.uint32(32).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestInputProposalResponse {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestInputProposalResponse();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.proposalCounter = longToNumber(reader.uint64() as Long);
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.changes.push(ChangeValue.decode(reader, reader.uint32()));
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestInputProposalResponse {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      proposalCounter: isSet(object.proposalCounter) ? Number(object.proposalCounter) : 0,
      changes: Array.isArray(object?.changes) ? object.changes.map((e: any) => ChangeValue.fromJSON(e)) : [],
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: RequestInputProposalResponse): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.proposalCounter !== 0) {
      obj.proposalCounter = Math.round(message.proposalCounter);
    }
    if (message.changes?.length) {
      obj.changes = message.changes.map((e) => ChangeValue.toJSON(e));
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestInputProposalResponse>, I>>(base?: I): RequestInputProposalResponse {
    return RequestInputProposalResponse.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestInputProposalResponse>, I>>(object: I): RequestInputProposalResponse {
    const message = createBaseRequestInputProposalResponse();
    message.id = object.id ?? "";
    message.proposalCounter = object.proposalCounter ?? 0;
    message.changes = object.changes?.map((e) => ChangeValue.fromPartial(e)) || [];
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseDivergentBranch(): DivergentBranch {
  return { branch: 0, divergesAtCounter: 0 };
}

export const DivergentBranch = {
  encode(message: DivergentBranch, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.branch !== 0) {
      writer.uint32(8).uint64(message.branch);
    }
    if (message.divergesAtCounter !== 0) {
      writer.uint32(16).uint64(message.divergesAtCounter);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): DivergentBranch {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseDivergentBranch();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 8) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.divergesAtCounter = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): DivergentBranch {
    return {
      branch: isSet(object.branch) ? Number(object.branch) : 0,
      divergesAtCounter: isSet(object.divergesAtCounter) ? Number(object.divergesAtCounter) : 0,
    };
  },

  toJSON(message: DivergentBranch): unknown {
    const obj: any = {};
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    if (message.divergesAtCounter !== 0) {
      obj.divergesAtCounter = Math.round(message.divergesAtCounter);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<DivergentBranch>, I>>(base?: I): DivergentBranch {
    return DivergentBranch.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<DivergentBranch>, I>>(object: I): DivergentBranch {
    const message = createBaseDivergentBranch();
    message.branch = object.branch ?? 0;
    message.divergesAtCounter = object.divergesAtCounter ?? 0;
    return message;
  },
};

function createBaseBranch(): Branch {
  return { id: 0, sourceBranchIds: [], divergentBranches: [], divergesAtCounter: 0 };
}

export const Branch = {
  encode(message: Branch, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== 0) {
      writer.uint32(8).uint64(message.id);
    }
    writer.uint32(18).fork();
    for (const v of message.sourceBranchIds) {
      writer.uint64(v);
    }
    writer.ldelim();
    for (const v of message.divergentBranches) {
      DivergentBranch.encode(v!, writer.uint32(26).fork()).ldelim();
    }
    if (message.divergesAtCounter !== 0) {
      writer.uint32(32).uint64(message.divergesAtCounter);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): Branch {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseBranch();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 8) {
            break;
          }

          message.id = longToNumber(reader.uint64() as Long);
          continue;
        case 2:
          if (tag === 16) {
            message.sourceBranchIds.push(longToNumber(reader.uint64() as Long));

            continue;
          }

          if (tag === 18) {
            const end2 = reader.uint32() + reader.pos;
            while (reader.pos < end2) {
              message.sourceBranchIds.push(longToNumber(reader.uint64() as Long));
            }

            continue;
          }

          break;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.divergentBranches.push(DivergentBranch.decode(reader, reader.uint32()));
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.divergesAtCounter = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): Branch {
    return {
      id: isSet(object.id) ? Number(object.id) : 0,
      sourceBranchIds: Array.isArray(object?.sourceBranchIds) ? object.sourceBranchIds.map((e: any) => Number(e)) : [],
      divergentBranches: Array.isArray(object?.divergentBranches)
        ? object.divergentBranches.map((e: any) => DivergentBranch.fromJSON(e))
        : [],
      divergesAtCounter: isSet(object.divergesAtCounter) ? Number(object.divergesAtCounter) : 0,
    };
  },

  toJSON(message: Branch): unknown {
    const obj: any = {};
    if (message.id !== 0) {
      obj.id = Math.round(message.id);
    }
    if (message.sourceBranchIds?.length) {
      obj.sourceBranchIds = message.sourceBranchIds.map((e) => Math.round(e));
    }
    if (message.divergentBranches?.length) {
      obj.divergentBranches = message.divergentBranches.map((e) => DivergentBranch.toJSON(e));
    }
    if (message.divergesAtCounter !== 0) {
      obj.divergesAtCounter = Math.round(message.divergesAtCounter);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<Branch>, I>>(base?: I): Branch {
    return Branch.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<Branch>, I>>(object: I): Branch {
    const message = createBaseBranch();
    message.id = object.id ?? 0;
    message.sourceBranchIds = object.sourceBranchIds?.map((e) => e) || [];
    message.divergentBranches = object.divergentBranches?.map((e) => DivergentBranch.fromPartial(e)) || [];
    message.divergesAtCounter = object.divergesAtCounter ?? 0;
    return message;
  },
};

function createBaseEmpty(): Empty {
  return {};
}

export const Empty = {
  encode(_: Empty, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): Empty {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseEmpty();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(_: any): Empty {
    return {};
  },

  toJSON(_: Empty): unknown {
    const obj: any = {};
    return obj;
  },

  create<I extends Exact<DeepPartial<Empty>, I>>(base?: I): Empty {
    return Empty.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<Empty>, I>>(_: I): Empty {
    const message = createBaseEmpty();
    return message;
  },
};

function createBaseExecutionStatus(): ExecutionStatus {
  return { id: "", monotonicCounter: 0, branch: 0 };
}

export const ExecutionStatus = {
  encode(message: ExecutionStatus, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.monotonicCounter !== 0) {
      writer.uint32(16).uint64(message.monotonicCounter);
    }
    if (message.branch !== 0) {
      writer.uint32(24).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ExecutionStatus {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseExecutionStatus();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.monotonicCounter = longToNumber(reader.uint64() as Long);
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ExecutionStatus {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      monotonicCounter: isSet(object.monotonicCounter) ? Number(object.monotonicCounter) : 0,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: ExecutionStatus): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.monotonicCounter !== 0) {
      obj.monotonicCounter = Math.round(message.monotonicCounter);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ExecutionStatus>, I>>(base?: I): ExecutionStatus {
    return ExecutionStatus.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ExecutionStatus>, I>>(object: I): ExecutionStatus {
    const message = createBaseExecutionStatus();
    message.id = object.id ?? "";
    message.monotonicCounter = object.monotonicCounter ?? 0;
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseFileAddressedChangeValueWithCounter(): FileAddressedChangeValueWithCounter {
  return { id: "", nodeName: "", branch: 0, counter: 0, change: undefined };
}

export const FileAddressedChangeValueWithCounter = {
  encode(message: FileAddressedChangeValueWithCounter, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.nodeName !== "") {
      writer.uint32(18).string(message.nodeName);
    }
    if (message.branch !== 0) {
      writer.uint32(24).uint64(message.branch);
    }
    if (message.counter !== 0) {
      writer.uint32(32).uint64(message.counter);
    }
    if (message.change !== undefined) {
      ChangeValueWithCounter.encode(message.change, writer.uint32(42).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): FileAddressedChangeValueWithCounter {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseFileAddressedChangeValueWithCounter();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.nodeName = reader.string();
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.counter = longToNumber(reader.uint64() as Long);
          continue;
        case 5:
          if (tag !== 42) {
            break;
          }

          message.change = ChangeValueWithCounter.decode(reader, reader.uint32());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): FileAddressedChangeValueWithCounter {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      nodeName: isSet(object.nodeName) ? String(object.nodeName) : "",
      branch: isSet(object.branch) ? Number(object.branch) : 0,
      counter: isSet(object.counter) ? Number(object.counter) : 0,
      change: isSet(object.change) ? ChangeValueWithCounter.fromJSON(object.change) : undefined,
    };
  },

  toJSON(message: FileAddressedChangeValueWithCounter): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.nodeName !== "") {
      obj.nodeName = message.nodeName;
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    if (message.counter !== 0) {
      obj.counter = Math.round(message.counter);
    }
    if (message.change !== undefined) {
      obj.change = ChangeValueWithCounter.toJSON(message.change);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<FileAddressedChangeValueWithCounter>, I>>(
    base?: I,
  ): FileAddressedChangeValueWithCounter {
    return FileAddressedChangeValueWithCounter.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<FileAddressedChangeValueWithCounter>, I>>(
    object: I,
  ): FileAddressedChangeValueWithCounter {
    const message = createBaseFileAddressedChangeValueWithCounter();
    message.id = object.id ?? "";
    message.nodeName = object.nodeName ?? "";
    message.branch = object.branch ?? 0;
    message.counter = object.counter ?? 0;
    message.change = (object.change !== undefined && object.change !== null)
      ? ChangeValueWithCounter.fromPartial(object.change)
      : undefined;
    return message;
  },
};

function createBaseRequestOnlyId(): RequestOnlyId {
  return { id: "", branch: 0 };
}

export const RequestOnlyId = {
  encode(message: RequestOnlyId, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.branch !== 0) {
      writer.uint32(16).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestOnlyId {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestOnlyId();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestOnlyId {
    return { id: isSet(object.id) ? String(object.id) : "", branch: isSet(object.branch) ? Number(object.branch) : 0 };
  },

  toJSON(message: RequestOnlyId): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestOnlyId>, I>>(base?: I): RequestOnlyId {
    return RequestOnlyId.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestOnlyId>, I>>(object: I): RequestOnlyId {
    const message = createBaseRequestOnlyId();
    message.id = object.id ?? "";
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseFilteredPollNodeWillExecuteEventsRequest(): FilteredPollNodeWillExecuteEventsRequest {
  return { id: "" };
}

export const FilteredPollNodeWillExecuteEventsRequest = {
  encode(message: FilteredPollNodeWillExecuteEventsRequest, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): FilteredPollNodeWillExecuteEventsRequest {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseFilteredPollNodeWillExecuteEventsRequest();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): FilteredPollNodeWillExecuteEventsRequest {
    return { id: isSet(object.id) ? String(object.id) : "" };
  },

  toJSON(message: FilteredPollNodeWillExecuteEventsRequest): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<FilteredPollNodeWillExecuteEventsRequest>, I>>(
    base?: I,
  ): FilteredPollNodeWillExecuteEventsRequest {
    return FilteredPollNodeWillExecuteEventsRequest.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<FilteredPollNodeWillExecuteEventsRequest>, I>>(
    object: I,
  ): FilteredPollNodeWillExecuteEventsRequest {
    const message = createBaseFilteredPollNodeWillExecuteEventsRequest();
    message.id = object.id ?? "";
    return message;
  },
};

function createBaseRequestAtFrame(): RequestAtFrame {
  return { id: "", frame: 0, branch: 0 };
}

export const RequestAtFrame = {
  encode(message: RequestAtFrame, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.frame !== 0) {
      writer.uint32(16).uint64(message.frame);
    }
    if (message.branch !== 0) {
      writer.uint32(24).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestAtFrame {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestAtFrame();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.frame = longToNumber(reader.uint64() as Long);
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestAtFrame {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      frame: isSet(object.frame) ? Number(object.frame) : 0,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: RequestAtFrame): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.frame !== 0) {
      obj.frame = Math.round(message.frame);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestAtFrame>, I>>(base?: I): RequestAtFrame {
    return RequestAtFrame.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestAtFrame>, I>>(object: I): RequestAtFrame {
    const message = createBaseRequestAtFrame();
    message.id = object.id ?? "";
    message.frame = object.frame ?? 0;
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseRequestNewBranch(): RequestNewBranch {
  return { id: "", sourceBranchId: 0, divergesAtCounter: 0 };
}

export const RequestNewBranch = {
  encode(message: RequestNewBranch, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.sourceBranchId !== 0) {
      writer.uint32(16).uint64(message.sourceBranchId);
    }
    if (message.divergesAtCounter !== 0) {
      writer.uint32(24).uint64(message.divergesAtCounter);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestNewBranch {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestNewBranch();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 16) {
            break;
          }

          message.sourceBranchId = longToNumber(reader.uint64() as Long);
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.divergesAtCounter = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestNewBranch {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      sourceBranchId: isSet(object.sourceBranchId) ? Number(object.sourceBranchId) : 0,
      divergesAtCounter: isSet(object.divergesAtCounter) ? Number(object.divergesAtCounter) : 0,
    };
  },

  toJSON(message: RequestNewBranch): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.sourceBranchId !== 0) {
      obj.sourceBranchId = Math.round(message.sourceBranchId);
    }
    if (message.divergesAtCounter !== 0) {
      obj.divergesAtCounter = Math.round(message.divergesAtCounter);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestNewBranch>, I>>(base?: I): RequestNewBranch {
    return RequestNewBranch.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestNewBranch>, I>>(object: I): RequestNewBranch {
    const message = createBaseRequestNewBranch();
    message.id = object.id ?? "";
    message.sourceBranchId = object.sourceBranchId ?? 0;
    message.divergesAtCounter = object.divergesAtCounter ?? 0;
    return message;
  },
};

function createBaseRequestListBranches(): RequestListBranches {
  return { id: "" };
}

export const RequestListBranches = {
  encode(message: RequestListBranches, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestListBranches {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestListBranches();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestListBranches {
    return { id: isSet(object.id) ? String(object.id) : "" };
  },

  toJSON(message: RequestListBranches): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestListBranches>, I>>(base?: I): RequestListBranches {
    return RequestListBranches.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestListBranches>, I>>(object: I): RequestListBranches {
    const message = createBaseRequestListBranches();
    message.id = object.id ?? "";
    return message;
  },
};

function createBaseListBranchesRes(): ListBranchesRes {
  return { id: "", branches: [] };
}

export const ListBranchesRes = {
  encode(message: ListBranchesRes, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    for (const v of message.branches) {
      Branch.encode(v!, writer.uint32(18).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ListBranchesRes {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseListBranchesRes();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.branches.push(Branch.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ListBranchesRes {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      branches: Array.isArray(object?.branches) ? object.branches.map((e: any) => Branch.fromJSON(e)) : [],
    };
  },

  toJSON(message: ListBranchesRes): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.branches?.length) {
      obj.branches = message.branches.map((e) => Branch.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ListBranchesRes>, I>>(base?: I): ListBranchesRes {
    return ListBranchesRes.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ListBranchesRes>, I>>(object: I): ListBranchesRes {
    const message = createBaseListBranchesRes();
    message.id = object.id ?? "";
    message.branches = object.branches?.map((e) => Branch.fromPartial(e)) || [];
    return message;
  },
};

function createBaseRequestFileMerge(): RequestFileMerge {
  return { id: "", file: undefined, branch: 0 };
}

export const RequestFileMerge = {
  encode(message: RequestFileMerge, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.file !== undefined) {
      File.encode(message.file, writer.uint32(18).fork()).ldelim();
    }
    if (message.branch !== 0) {
      writer.uint32(24).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestFileMerge {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestFileMerge();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.file = File.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestFileMerge {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      file: isSet(object.file) ? File.fromJSON(object.file) : undefined,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: RequestFileMerge): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.file !== undefined) {
      obj.file = File.toJSON(message.file);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestFileMerge>, I>>(base?: I): RequestFileMerge {
    return RequestFileMerge.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestFileMerge>, I>>(object: I): RequestFileMerge {
    const message = createBaseRequestFileMerge();
    message.id = object.id ?? "";
    message.file = (object.file !== undefined && object.file !== null) ? File.fromPartial(object.file) : undefined;
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseParquetFile(): ParquetFile {
  return { data: new Uint8Array(0) };
}

export const ParquetFile = {
  encode(message: ParquetFile, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.data.length !== 0) {
      writer.uint32(10).bytes(message.data);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ParquetFile {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseParquetFile();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.data = reader.bytes();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ParquetFile {
    return { data: isSet(object.data) ? bytesFromBase64(object.data) : new Uint8Array(0) };
  },

  toJSON(message: ParquetFile): unknown {
    const obj: any = {};
    if (message.data.length !== 0) {
      obj.data = base64FromBytes(message.data);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ParquetFile>, I>>(base?: I): ParquetFile {
    return ParquetFile.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ParquetFile>, I>>(object: I): ParquetFile {
    const message = createBaseParquetFile();
    message.data = object.data ?? new Uint8Array(0);
    return message;
  },
};

function createBaseQueryAtFrame(): QueryAtFrame {
  return { id: "", query: undefined, frame: 0, branch: 0 };
}

export const QueryAtFrame = {
  encode(message: QueryAtFrame, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.query !== undefined) {
      Query.encode(message.query, writer.uint32(18).fork()).ldelim();
    }
    if (message.frame !== 0) {
      writer.uint32(24).uint64(message.frame);
    }
    if (message.branch !== 0) {
      writer.uint32(32).uint64(message.branch);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): QueryAtFrame {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseQueryAtFrame();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.query = Query.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.frame = longToNumber(reader.uint64() as Long);
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): QueryAtFrame {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      query: isSet(object.query) ? Query.fromJSON(object.query) : undefined,
      frame: isSet(object.frame) ? Number(object.frame) : 0,
      branch: isSet(object.branch) ? Number(object.branch) : 0,
    };
  },

  toJSON(message: QueryAtFrame): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.query !== undefined) {
      obj.query = Query.toJSON(message.query);
    }
    if (message.frame !== 0) {
      obj.frame = Math.round(message.frame);
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<QueryAtFrame>, I>>(base?: I): QueryAtFrame {
    return QueryAtFrame.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<QueryAtFrame>, I>>(object: I): QueryAtFrame {
    const message = createBaseQueryAtFrame();
    message.id = object.id ?? "";
    message.query = (object.query !== undefined && object.query !== null) ? Query.fromPartial(object.query) : undefined;
    message.frame = object.frame ?? 0;
    message.branch = object.branch ?? 0;
    return message;
  },
};

function createBaseQueryAtFrameResponse(): QueryAtFrameResponse {
  return { values: [] };
}

export const QueryAtFrameResponse = {
  encode(message: QueryAtFrameResponse, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.values) {
      WrappedChangeValue.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): QueryAtFrameResponse {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseQueryAtFrameResponse();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.values.push(WrappedChangeValue.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): QueryAtFrameResponse {
    return {
      values: Array.isArray(object?.values) ? object.values.map((e: any) => WrappedChangeValue.fromJSON(e)) : [],
    };
  },

  toJSON(message: QueryAtFrameResponse): unknown {
    const obj: any = {};
    if (message.values?.length) {
      obj.values = message.values.map((e) => WrappedChangeValue.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<QueryAtFrameResponse>, I>>(base?: I): QueryAtFrameResponse {
    return QueryAtFrameResponse.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<QueryAtFrameResponse>, I>>(object: I): QueryAtFrameResponse {
    const message = createBaseQueryAtFrameResponse();
    message.values = object.values?.map((e) => WrappedChangeValue.fromPartial(e)) || [];
    return message;
  },
};

function createBaseRequestAckNodeWillExecuteEvent(): RequestAckNodeWillExecuteEvent {
  return { id: "", branch: 0, counter: 0 };
}

export const RequestAckNodeWillExecuteEvent = {
  encode(message: RequestAckNodeWillExecuteEvent, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.id !== "") {
      writer.uint32(10).string(message.id);
    }
    if (message.branch !== 0) {
      writer.uint32(24).uint64(message.branch);
    }
    if (message.counter !== 0) {
      writer.uint32(32).uint64(message.counter);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RequestAckNodeWillExecuteEvent {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRequestAckNodeWillExecuteEvent();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.id = reader.string();
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.branch = longToNumber(reader.uint64() as Long);
          continue;
        case 4:
          if (tag !== 32) {
            break;
          }

          message.counter = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RequestAckNodeWillExecuteEvent {
    return {
      id: isSet(object.id) ? String(object.id) : "",
      branch: isSet(object.branch) ? Number(object.branch) : 0,
      counter: isSet(object.counter) ? Number(object.counter) : 0,
    };
  },

  toJSON(message: RequestAckNodeWillExecuteEvent): unknown {
    const obj: any = {};
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.branch !== 0) {
      obj.branch = Math.round(message.branch);
    }
    if (message.counter !== 0) {
      obj.counter = Math.round(message.counter);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RequestAckNodeWillExecuteEvent>, I>>(base?: I): RequestAckNodeWillExecuteEvent {
    return RequestAckNodeWillExecuteEvent.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RequestAckNodeWillExecuteEvent>, I>>(
    object: I,
  ): RequestAckNodeWillExecuteEvent {
    const message = createBaseRequestAckNodeWillExecuteEvent();
    message.id = object.id ?? "";
    message.branch = object.branch ?? 0;
    message.counter = object.counter ?? 0;
    return message;
  },
};

function createBaseRespondPollNodeWillExecuteEvents(): RespondPollNodeWillExecuteEvents {
  return { nodeWillExecuteEvents: [] };
}

export const RespondPollNodeWillExecuteEvents = {
  encode(message: RespondPollNodeWillExecuteEvents, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.nodeWillExecuteEvents) {
      NodeWillExecuteOnBranch.encode(v!, writer.uint32(10).fork()).ldelim();
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): RespondPollNodeWillExecuteEvents {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseRespondPollNodeWillExecuteEvents();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.nodeWillExecuteEvents.push(NodeWillExecuteOnBranch.decode(reader, reader.uint32()));
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): RespondPollNodeWillExecuteEvents {
    return {
      nodeWillExecuteEvents: Array.isArray(object?.nodeWillExecuteEvents)
        ? object.nodeWillExecuteEvents.map((e: any) => NodeWillExecuteOnBranch.fromJSON(e))
        : [],
    };
  },

  toJSON(message: RespondPollNodeWillExecuteEvents): unknown {
    const obj: any = {};
    if (message.nodeWillExecuteEvents?.length) {
      obj.nodeWillExecuteEvents = message.nodeWillExecuteEvents.map((e) => NodeWillExecuteOnBranch.toJSON(e));
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<RespondPollNodeWillExecuteEvents>, I>>(
    base?: I,
  ): RespondPollNodeWillExecuteEvents {
    return RespondPollNodeWillExecuteEvents.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<RespondPollNodeWillExecuteEvents>, I>>(
    object: I,
  ): RespondPollNodeWillExecuteEvents {
    const message = createBaseRespondPollNodeWillExecuteEvents();
    message.nodeWillExecuteEvents = object.nodeWillExecuteEvents?.map((e) => NodeWillExecuteOnBranch.fromPartial(e)) ||
      [];
    return message;
  },
};

function createBasePromptLibraryRecord(): PromptLibraryRecord {
  return { record: undefined, versionCounter: 0 };
}

export const PromptLibraryRecord = {
  encode(message: PromptLibraryRecord, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.record !== undefined) {
      UpsertPromptLibraryRecord.encode(message.record, writer.uint32(10).fork()).ldelim();
    }
    if (message.versionCounter !== 0) {
      writer.uint32(24).uint64(message.versionCounter);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): PromptLibraryRecord {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBasePromptLibraryRecord();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.record = UpsertPromptLibraryRecord.decode(reader, reader.uint32());
          continue;
        case 3:
          if (tag !== 24) {
            break;
          }

          message.versionCounter = longToNumber(reader.uint64() as Long);
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): PromptLibraryRecord {
    return {
      record: isSet(object.record) ? UpsertPromptLibraryRecord.fromJSON(object.record) : undefined,
      versionCounter: isSet(object.versionCounter) ? Number(object.versionCounter) : 0,
    };
  },

  toJSON(message: PromptLibraryRecord): unknown {
    const obj: any = {};
    if (message.record !== undefined) {
      obj.record = UpsertPromptLibraryRecord.toJSON(message.record);
    }
    if (message.versionCounter !== 0) {
      obj.versionCounter = Math.round(message.versionCounter);
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<PromptLibraryRecord>, I>>(base?: I): PromptLibraryRecord {
    return PromptLibraryRecord.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<PromptLibraryRecord>, I>>(object: I): PromptLibraryRecord {
    const message = createBasePromptLibraryRecord();
    message.record = (object.record !== undefined && object.record !== null)
      ? UpsertPromptLibraryRecord.fromPartial(object.record)
      : undefined;
    message.versionCounter = object.versionCounter ?? 0;
    return message;
  },
};

function createBaseUpsertPromptLibraryRecord(): UpsertPromptLibraryRecord {
  return { template: "", name: "", id: "", description: undefined };
}

export const UpsertPromptLibraryRecord = {
  encode(message: UpsertPromptLibraryRecord, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    if (message.template !== "") {
      writer.uint32(10).string(message.template);
    }
    if (message.name !== "") {
      writer.uint32(18).string(message.name);
    }
    if (message.id !== "") {
      writer.uint32(26).string(message.id);
    }
    if (message.description !== undefined) {
      writer.uint32(34).string(message.description);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): UpsertPromptLibraryRecord {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseUpsertPromptLibraryRecord();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.template = reader.string();
          continue;
        case 2:
          if (tag !== 18) {
            break;
          }

          message.name = reader.string();
          continue;
        case 3:
          if (tag !== 26) {
            break;
          }

          message.id = reader.string();
          continue;
        case 4:
          if (tag !== 34) {
            break;
          }

          message.description = reader.string();
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): UpsertPromptLibraryRecord {
    return {
      template: isSet(object.template) ? String(object.template) : "",
      name: isSet(object.name) ? String(object.name) : "",
      id: isSet(object.id) ? String(object.id) : "",
      description: isSet(object.description) ? String(object.description) : undefined,
    };
  },

  toJSON(message: UpsertPromptLibraryRecord): unknown {
    const obj: any = {};
    if (message.template !== "") {
      obj.template = message.template;
    }
    if (message.name !== "") {
      obj.name = message.name;
    }
    if (message.id !== "") {
      obj.id = message.id;
    }
    if (message.description !== undefined) {
      obj.description = message.description;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<UpsertPromptLibraryRecord>, I>>(base?: I): UpsertPromptLibraryRecord {
    return UpsertPromptLibraryRecord.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<UpsertPromptLibraryRecord>, I>>(object: I): UpsertPromptLibraryRecord {
    const message = createBaseUpsertPromptLibraryRecord();
    message.template = object.template ?? "";
    message.name = object.name ?? "";
    message.id = object.id ?? "";
    message.description = object.description ?? undefined;
    return message;
  },
};

function createBaseListRegisteredGraphsResponse(): ListRegisteredGraphsResponse {
  return { ids: [] };
}

export const ListRegisteredGraphsResponse = {
  encode(message: ListRegisteredGraphsResponse, writer: _m0.Writer = _m0.Writer.create()): _m0.Writer {
    for (const v of message.ids) {
      writer.uint32(10).string(v!);
    }
    return writer;
  },

  decode(input: _m0.Reader | Uint8Array, length?: number): ListRegisteredGraphsResponse {
    const reader = input instanceof _m0.Reader ? input : _m0.Reader.create(input);
    let end = length === undefined ? reader.len : reader.pos + length;
    const message = createBaseListRegisteredGraphsResponse();
    while (reader.pos < end) {
      const tag = reader.uint32();
      switch (tag >>> 3) {
        case 1:
          if (tag !== 10) {
            break;
          }

          message.ids.push(reader.string());
          continue;
      }
      if ((tag & 7) === 4 || tag === 0) {
        break;
      }
      reader.skipType(tag & 7);
    }
    return message;
  },

  fromJSON(object: any): ListRegisteredGraphsResponse {
    return { ids: Array.isArray(object?.ids) ? object.ids.map((e: any) => String(e)) : [] };
  },

  toJSON(message: ListRegisteredGraphsResponse): unknown {
    const obj: any = {};
    if (message.ids?.length) {
      obj.ids = message.ids;
    }
    return obj;
  },

  create<I extends Exact<DeepPartial<ListRegisteredGraphsResponse>, I>>(base?: I): ListRegisteredGraphsResponse {
    return ListRegisteredGraphsResponse.fromPartial(base ?? ({} as any));
  },
  fromPartial<I extends Exact<DeepPartial<ListRegisteredGraphsResponse>, I>>(object: I): ListRegisteredGraphsResponse {
    const message = createBaseListRegisteredGraphsResponse();
    message.ids = object.ids?.map((e) => e) || [];
    return message;
  },
};

/** API: */
export interface ExecutionRuntime {
  RunQuery(request: QueryAtFrame): Promise<QueryAtFrameResponse>;
  /** Merge a new file - if an existing file is available at the id, will merge the new file into the existing one */
  Merge(request: RequestFileMerge): Promise<ExecutionStatus>;
  /** Get the current graph state of a file at a branch and counter position */
  CurrentFileState(request: RequestOnlyId): Promise<File>;
  /** Get the parquet history for a specific branch and Id - returns bytes */
  GetParquetHistory(request: RequestOnlyId): Promise<ParquetFile>;
  /** Resume execution */
  Play(request: RequestAtFrame): Promise<ExecutionStatus>;
  /** Pause execution */
  Pause(request: RequestAtFrame): Promise<ExecutionStatus>;
  /** Split history into a separate branch */
  Branch(request: RequestNewBranch): Promise<ExecutionStatus>;
  /** Get all branches */
  ListBranches(request: RequestListBranches): Promise<ListBranchesRes>;
  /** List all registered files */
  ListRegisteredGraphs(request: Empty): Promise<ListRegisteredGraphsResponse>;
  /** Receive a stream of input proposals <- this is a server-side stream */
  ListInputProposals(request: RequestOnlyId): Observable<InputProposal>;
  /** Push responses to input proposals (these wait for some input from a host until they're resolved) <- RPC client to server */
  RespondToInputProposal(request: RequestInputProposalResponse): Promise<Empty>;
  /** Observe the stream of execution events <- this is a server-side stream */
  ListChangeEvents(request: RequestOnlyId): Observable<ChangeValueWithCounter>;
  ListNodeWillExecuteEvents(request: RequestOnlyId): Observable<NodeWillExecuteOnBranch>;
  /** Observe when the server thinks our local node implementation should execute and with what changes */
  PollCustomNodeWillExecuteEvents(
    request: FilteredPollNodeWillExecuteEventsRequest,
  ): Promise<RespondPollNodeWillExecuteEvents>;
  AckNodeWillExecuteEvent(request: RequestAckNodeWillExecuteEvent): Promise<ExecutionStatus>;
  /** Receive events from workers <- this is an RPC client to server, we don't need to wait for a response from the server */
  PushWorkerEvent(request: FileAddressedChangeValueWithCounter): Promise<ExecutionStatus>;
  PushTemplatePartial(request: UpsertPromptLibraryRecord): Promise<ExecutionStatus>;
}

export const ExecutionRuntimeServiceName = "promptgraph.ExecutionRuntime";
export class ExecutionRuntimeClientImpl implements ExecutionRuntime {
  private readonly rpc: Rpc;
  private readonly service: string;
  constructor(rpc: Rpc, opts?: { service?: string }) {
    this.service = opts?.service || ExecutionRuntimeServiceName;
    this.rpc = rpc;
    this.RunQuery = this.RunQuery.bind(this);
    this.Merge = this.Merge.bind(this);
    this.CurrentFileState = this.CurrentFileState.bind(this);
    this.GetParquetHistory = this.GetParquetHistory.bind(this);
    this.Play = this.Play.bind(this);
    this.Pause = this.Pause.bind(this);
    this.Branch = this.Branch.bind(this);
    this.ListBranches = this.ListBranches.bind(this);
    this.ListRegisteredGraphs = this.ListRegisteredGraphs.bind(this);
    this.ListInputProposals = this.ListInputProposals.bind(this);
    this.RespondToInputProposal = this.RespondToInputProposal.bind(this);
    this.ListChangeEvents = this.ListChangeEvents.bind(this);
    this.ListNodeWillExecuteEvents = this.ListNodeWillExecuteEvents.bind(this);
    this.PollCustomNodeWillExecuteEvents = this.PollCustomNodeWillExecuteEvents.bind(this);
    this.AckNodeWillExecuteEvent = this.AckNodeWillExecuteEvent.bind(this);
    this.PushWorkerEvent = this.PushWorkerEvent.bind(this);
    this.PushTemplatePartial = this.PushTemplatePartial.bind(this);
  }
  RunQuery(request: QueryAtFrame): Promise<QueryAtFrameResponse> {
    const data = QueryAtFrame.encode(request).finish();
    const promise = this.rpc.request(this.service, "RunQuery", data);
    return promise.then((data) => QueryAtFrameResponse.decode(_m0.Reader.create(data)));
  }

  Merge(request: RequestFileMerge): Promise<ExecutionStatus> {
    const data = RequestFileMerge.encode(request).finish();
    const promise = this.rpc.request(this.service, "Merge", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }

  CurrentFileState(request: RequestOnlyId): Promise<File> {
    const data = RequestOnlyId.encode(request).finish();
    const promise = this.rpc.request(this.service, "CurrentFileState", data);
    return promise.then((data) => File.decode(_m0.Reader.create(data)));
  }

  GetParquetHistory(request: RequestOnlyId): Promise<ParquetFile> {
    const data = RequestOnlyId.encode(request).finish();
    const promise = this.rpc.request(this.service, "GetParquetHistory", data);
    return promise.then((data) => ParquetFile.decode(_m0.Reader.create(data)));
  }

  Play(request: RequestAtFrame): Promise<ExecutionStatus> {
    const data = RequestAtFrame.encode(request).finish();
    const promise = this.rpc.request(this.service, "Play", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }

  Pause(request: RequestAtFrame): Promise<ExecutionStatus> {
    const data = RequestAtFrame.encode(request).finish();
    const promise = this.rpc.request(this.service, "Pause", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }

  Branch(request: RequestNewBranch): Promise<ExecutionStatus> {
    const data = RequestNewBranch.encode(request).finish();
    const promise = this.rpc.request(this.service, "Branch", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }

  ListBranches(request: RequestListBranches): Promise<ListBranchesRes> {
    const data = RequestListBranches.encode(request).finish();
    const promise = this.rpc.request(this.service, "ListBranches", data);
    return promise.then((data) => ListBranchesRes.decode(_m0.Reader.create(data)));
  }

  ListRegisteredGraphs(request: Empty): Promise<ListRegisteredGraphsResponse> {
    const data = Empty.encode(request).finish();
    const promise = this.rpc.request(this.service, "ListRegisteredGraphs", data);
    return promise.then((data) => ListRegisteredGraphsResponse.decode(_m0.Reader.create(data)));
  }

  ListInputProposals(request: RequestOnlyId): Observable<InputProposal> {
    const data = RequestOnlyId.encode(request).finish();
    const result = this.rpc.serverStreamingRequest(this.service, "ListInputProposals", data);
    return result.pipe(map((data) => InputProposal.decode(_m0.Reader.create(data))));
  }

  RespondToInputProposal(request: RequestInputProposalResponse): Promise<Empty> {
    const data = RequestInputProposalResponse.encode(request).finish();
    const promise = this.rpc.request(this.service, "RespondToInputProposal", data);
    return promise.then((data) => Empty.decode(_m0.Reader.create(data)));
  }

  ListChangeEvents(request: RequestOnlyId): Observable<ChangeValueWithCounter> {
    const data = RequestOnlyId.encode(request).finish();
    const result = this.rpc.serverStreamingRequest(this.service, "ListChangeEvents", data);
    return result.pipe(map((data) => ChangeValueWithCounter.decode(_m0.Reader.create(data))));
  }

  ListNodeWillExecuteEvents(request: RequestOnlyId): Observable<NodeWillExecuteOnBranch> {
    const data = RequestOnlyId.encode(request).finish();
    const result = this.rpc.serverStreamingRequest(this.service, "ListNodeWillExecuteEvents", data);
    return result.pipe(map((data) => NodeWillExecuteOnBranch.decode(_m0.Reader.create(data))));
  }

  PollCustomNodeWillExecuteEvents(
    request: FilteredPollNodeWillExecuteEventsRequest,
  ): Promise<RespondPollNodeWillExecuteEvents> {
    const data = FilteredPollNodeWillExecuteEventsRequest.encode(request).finish();
    const promise = this.rpc.request(this.service, "PollCustomNodeWillExecuteEvents", data);
    return promise.then((data) => RespondPollNodeWillExecuteEvents.decode(_m0.Reader.create(data)));
  }

  AckNodeWillExecuteEvent(request: RequestAckNodeWillExecuteEvent): Promise<ExecutionStatus> {
    const data = RequestAckNodeWillExecuteEvent.encode(request).finish();
    const promise = this.rpc.request(this.service, "AckNodeWillExecuteEvent", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }

  PushWorkerEvent(request: FileAddressedChangeValueWithCounter): Promise<ExecutionStatus> {
    const data = FileAddressedChangeValueWithCounter.encode(request).finish();
    const promise = this.rpc.request(this.service, "PushWorkerEvent", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }

  PushTemplatePartial(request: UpsertPromptLibraryRecord): Promise<ExecutionStatus> {
    const data = UpsertPromptLibraryRecord.encode(request).finish();
    const promise = this.rpc.request(this.service, "PushTemplatePartial", data);
    return promise.then((data) => ExecutionStatus.decode(_m0.Reader.create(data)));
  }
}

interface Rpc {
  request(service: string, method: string, data: Uint8Array): Promise<Uint8Array>;
  clientStreamingRequest(service: string, method: string, data: Observable<Uint8Array>): Promise<Uint8Array>;
  serverStreamingRequest(service: string, method: string, data: Uint8Array): Observable<Uint8Array>;
  bidirectionalStreamingRequest(service: string, method: string, data: Observable<Uint8Array>): Observable<Uint8Array>;
}

declare const self: any | undefined;
declare const window: any | undefined;
declare const global: any | undefined;
const tsProtoGlobalThis: any = (() => {
  if (typeof globalThis !== "undefined") {
    return globalThis;
  }
  if (typeof self !== "undefined") {
    return self;
  }
  if (typeof window !== "undefined") {
    return window;
  }
  if (typeof global !== "undefined") {
    return global;
  }
  throw "Unable to locate global object";
})();

function bytesFromBase64(b64: string): Uint8Array {
  if (tsProtoGlobalThis.Buffer) {
    return Uint8Array.from(tsProtoGlobalThis.Buffer.from(b64, "base64"));
  } else {
    const bin = tsProtoGlobalThis.atob(b64);
    const arr = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; ++i) {
      arr[i] = bin.charCodeAt(i);
    }
    return arr;
  }
}

function base64FromBytes(arr: Uint8Array): string {
  if (tsProtoGlobalThis.Buffer) {
    return tsProtoGlobalThis.Buffer.from(arr).toString("base64");
  } else {
    const bin: string[] = [];
    arr.forEach((byte) => {
      bin.push(String.fromCharCode(byte));
    });
    return tsProtoGlobalThis.btoa(bin.join(""));
  }
}

type Builtin = Date | Function | Uint8Array | string | number | boolean | undefined;

export type DeepPartial<T> = T extends Builtin ? T
  : T extends Array<infer U> ? Array<DeepPartial<U>> : T extends ReadonlyArray<infer U> ? ReadonlyArray<DeepPartial<U>>
  : T extends {} ? { [K in keyof T]?: DeepPartial<T[K]> }
  : Partial<T>;

type KeysOfUnion<T> = T extends T ? keyof T : never;
export type Exact<P, I extends P> = P extends Builtin ? P
  : P & { [K in keyof P]: Exact<P[K], I[K]> } & { [K in Exclude<keyof I, KeysOfUnion<P>>]: never };

function longToNumber(long: Long): number {
  if (long.gt(Number.MAX_SAFE_INTEGER)) {
    throw new tsProtoGlobalThis.Error("Value is larger than Number.MAX_SAFE_INTEGER");
  }
  return long.toNumber();
}

if (_m0.util.Long !== Long) {
  _m0.util.Long = Long as any;
  _m0.configure();
}

function isObject(value: any): boolean {
  return typeof value === "object" && value !== null;
}

function isSet(value: any): boolean {
  return value !== null && value !== undefined;
}
