<script lang="ts">
  import "../app.css";
  import * as Resizable from "@/components/ui/resizable";
  import { primaryRoutes } from "@/config";
  import Nav from "@/components/Nav.svelte";
  import { Separator } from "@/components/ui/separator";
  import TopNav from "@/components/TopNav.svelte";

  export let defaultLayout = [265, 440, 655];
  export let defaultCollapsed = false;
  export let navCollapsedSize: number;

  let isCollapsed = defaultCollapsed;

  function onLayoutChange(sizes: number[]) {
    document.cookie = `PaneForge:layout=${JSON.stringify(sizes)}`;
  }

  function onCollapse() {
    isCollapsed = true;
    document.cookie = `PaneForge:collapsed=${true}`;
  }

  function onExpand() {
    isCollapsed = false;
    document.cookie = `PaneForge:collapsed=${false}`;
  }
</script>

<!--<div class="md:hidden">-->
<!--    <img src={MailLight} width={1280} height={1114} alt="Mail" class="block dark:hidden" />-->
<!--    <img src={MailDark} width={1280} height={1114} alt="Mail" class="hidden dark:block" />-->
<!--</div>-->
<div class="hidden md:block h-[100vh]">
    <div class="h-10">
        <TopNav />
    </div>
    <div class="h-[calc(100vh-40px)]">
        <Resizable.PaneGroup
                direction="horizontal"
                {onLayoutChange}
                class="h-full items-stretch"
        >
            <Resizable.Pane
                    defaultSize={defaultLayout[0]}
                    collapsedSize={navCollapsedSize}
                    collapsible
                    minSize={15}
                    maxSize={20}
                    {onCollapse}
                    {onExpand}
            >
                <Separator />
                <Nav {isCollapsed} routes={primaryRoutes} />
            </Resizable.Pane>
            <Resizable.Handle withHandle />
            <Resizable.Pane defaultSize={defaultLayout[1]} minSize={30}>
                <div class="p-8 w-full h-full">
                    <slot />
                </div>
            </Resizable.Pane>
        </Resizable.PaneGroup>
    </div>
</div>
