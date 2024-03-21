<script lang="ts">
    import {onDestroy, onMount} from 'svelte';
    import { EditorState } from '@codemirror/state';
    import { EditorView } from '@codemirror/view';
    import {basicSetup} from "codemirror";

    export let initialText = '';

    let editorDiv: HTMLDivElement | null = null;
    let editorView: EditorView | null = null;

    onMount(() => {
        if (!editorDiv) {
            throw new Error('Editor div not found');
        }
        const state = EditorState.create({
            doc: initialText,
            extensions: [basicSetup],
        });

        editorView = new EditorView({
            state,
            parent: editorDiv,
        });
    });

    // Cleanup the editor instance when the component is destroyed
    onDestroy(() => {
        editorView?.destroy();
    });
</script>

<div bind:this={editorDiv}></div>
