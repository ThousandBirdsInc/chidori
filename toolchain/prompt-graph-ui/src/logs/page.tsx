import * as React from "react";
import {useEffect} from "react";

import {ColumnDef, flexRender, getCoreRowModel, useReactTable} from "@tanstack/react-table"

import {Table, TableBody, TableCell, TableHead, TableHeader, TableRow} from "@/components/ui/table"
import {useStore} from "@/stores/store";
import {Input} from "@/components/ui/input";
import {ChangeValueWithCounter, NodeWillExecuteOnBranch} from "@/protobufs/DSL_v1";
import {TableRowChangeValue} from "@/components/data/TableRowChangeValue";
import {DisplaySerializedValue} from "@/components/data/DisplaySerializedValue";


export const nodeWillExecuteEventColumns: ColumnDef<NodeWillExecuteOnBranch>[] = [
  {
    accessorFn: (x) => x.counter,
    header: "ID",
  },
  {
    accessorFn: (x) => x.node?.sourceNode,
    header: "Node",
  },
  {
    accessorFn: (x) => x.node?.changeValuesUsedInExecution,
    header: "Execution Values",
    cell: ({ value, row: { original }, }) => {

      return <div>
        <Table className={"w-2/3"}>
          <TableHeader>
            <TableRow>
              <TableHead className={"p-2"}>Id</TableHead>
              <TableHead className={"p-2"}>Path</TableHead>
              <TableHead className={"p-2"}>Value</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {original.node?.changeValuesUsedInExecution?.map((x) => {
              return <TableRow>
                <TableCell className={"p-2"}>{x.monotonicCounter}</TableCell>
                <TableCell className={"p-2"}>{x.changeValue?.path?.address.join(":")}</TableCell>
                <TableCell className={"p-2"}>
                  <DisplaySerializedValue value={x.changeValue?.value}/>
                </TableCell>
              </TableRow>
            })}
          </TableBody>
        </Table>
      </div>;
    },
  },
]


  // {
  //   id: key + "",
  //     source_node: value.sourceNode,
  //   path: filledValue.path?.address.join(":") || "",
  //   value: JSON.stringify(filledValue.value),
  // }
export const changeEventColumns: ColumnDef<ChangeValueWithCounter>[] = [
  {
    accessorFn: (x) => x.monotonicCounter,
    header: "ID",
  },
  {
    accessorFn: (x) => x.sourceNode,
    header: "Node",
  },
  {
    accessorFn: (x) => x.filledValues,
    header: "Path",
    cell: function ({value, row: {original},}) {
        return <div>
          <Table className={"w-full"}>
            <TableHeader>
              <TableRow>
                <TableHead>Path</TableHead>
                <TableHead>Value</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {original.filledValues?.map((x) => (
                <TableRowChangeValue changeValue={x}/>
              ))}
            </TableBody>
          </Table>
      </div>;
    },
  }
]

interface DataTableProps<TData, TValue> {
  columns: ColumnDef<TData, TValue>[]
  data: TData[]
}

function DataTable<TData, TValue>(
  { columns, data, }: DataTableProps<TData, TValue>
) {
  const table = useReactTable({
    data,
    columns,
    getCoreRowModel: getCoreRowModel(),
  })

  return (
    <div className="rounded-md border w-full">
      <Table>
        <TableHeader>
          {table.getHeaderGroups().map((headerGroup) => (
            <TableRow key={headerGroup.id}>
              {headerGroup.headers.map((header) => {
                return (
                  <TableHead key={header.id}>
                    {header.isPlaceholder
                      ? null
                      : flexRender(
                        header.column.columnDef.header,
                        header.getContext()
                      )}
                  </TableHead>
                )
              })}
            </TableRow>
          ))}
        </TableHeader>
        <TableBody>
          {table.getRowModel().rows?.length ? (
            table.getRowModel().rows.map((row) => (
              <TableRow
                key={row.id}
                data-state={row.getIsSelected() && "selected"}
              >
                {row.getVisibleCells().map((cell) => (
                  <TableCell key={cell.id}>
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </TableCell>
                ))}
              </TableRow>
            ))
          ) : (
            <TableRow>
              <TableCell colSpan={columns.length} className="h-24 text-center">
                No results.
              </TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
    </div>
  )
}


function NodeWillExecuteEvents() {
  const nodeWillExecuteEvents = useStore(x => x.nodeWillExecuteEvents)

  const data = [] as any[];
  for (const [key, value] of Object.entries(nodeWillExecuteEvents)) {
    data.push(value)
  }

  return (
    <>
      <DataTable columns={nodeWillExecuteEventColumns} data={data}/>
    </>
  )
}


function ChangeEvents() {
  const changeEvents = useStore(x => x.changeEvents)

  const data = [] as ChangeValueWithCounter[];
  for (const [key, value] of Object.entries(changeEvents)) {
    data.push(value)
  }

  return (
    <>
      <DataTable columns={changeEventColumns} data={data}/>
    </>
  )
}


export default function LogsPage() {
  const fetchInitialState = useStore(x => x.fetchInitialState)

  useEffect(() => {
    fetchInitialState();
  }, [])

  return (
    <>
      <div className="flex flex-col md:items-center md:justify-between md:space-x-4 p-4 gap-4">
        <div className="flex-1">
          <div>
            <Input
              type="search"
              placeholder="Search..."
              className="md:w-[100px] lg:w-[300px]"
            />
          </div>
        </div>
        <NodeWillExecuteEvents/>
        <ChangeEvents/>
      </div>
    </>
  )
}
