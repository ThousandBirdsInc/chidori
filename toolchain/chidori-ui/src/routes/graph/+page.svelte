<script lang="ts">
  import { invoke } from '@tauri-apps/api/tauri';
  import { writable } from 'svelte/store';
  import {
    SvelteFlow,
    Controls,
    Background,
    BackgroundVariant,
    MiniMap
  } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';

  const nodes = writable([ ]);
  const edges = writable([ ]);

  invoke('get_graph_state').then((data) => {
    console.log(data)
    let d = JSON.parse(data);
    nodes.set(d.nodes);
    edges.set(d.edges);
  })


  const snapGrid: [number, number] = [25, 25];
</script>

<!--
👇 By default, the Svelte Flow container has a height of 100%.
This means that the parent container needs a height to render the flow.
-->
<div class="w-full h-full">
    <SvelteFlow
            {nodes}
            {edges}
            {snapGrid}
            fitView
            on:nodeclick={(event) => console.log('on node click', event.detail.node)}
    >
        <Controls />
        <Background variant={BackgroundVariant.Dots} />
        <MiniMap />
    </SvelteFlow>
</div>