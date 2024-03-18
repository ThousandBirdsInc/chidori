<script lang="ts">
  import * as Menubar from "$lib/components/ui/menubar";
</script>

<Menubar.Root>
    <Menubar.Menu>
        <Menubar.Trigger>File</Menubar.Trigger>
        <Menubar.Content>
            <Menubar.Item>
                Open
                <Menubar.Shortcut>⌘O</Menubar.Shortcut>
            </Menubar.Item>
            <Menubar.Separator />
            <Menubar.Item>Close</Menubar.Item>
        </Menubar.Content>
    </Menubar.Menu>
</Menubar.Root>
