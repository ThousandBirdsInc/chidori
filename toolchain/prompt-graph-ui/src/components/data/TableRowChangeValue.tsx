import * as React from "react";
import {ChangeValue} from "@/protobufs/DSL_v1";
import {Table, TableHeader, TableHead, TableBody, TableRow, TableCell} from "@/components/ui/table";
import {DisplaySerializedValue} from "@/components/data/DisplaySerializedValue";

export const TableRowChangeValue = (props: {changeValue: ChangeValue}) => {
  const {changeValue} = props;
  // return <div>
  //   <Table>
  //     <TableHeader>
  //       <TableRow>
  //         <TableHead>Path</TableHead>
  //         <TableHead>Value</TableHead>
  //       </TableRow>
  //     </TableHeader>
  //     <TableBody>
  //     </TableBody>
  //   </Table>
  //
  // </div>
  //
  return <TableRow>
    <TableCell>{changeValue.path?.address.join(":")}</TableCell>
    <TableCell>
      <DisplaySerializedValue value={changeValue.value}/>
    </TableCell>
  </TableRow>
}