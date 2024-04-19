<script>
    export let cell;
</script>

<div>
    <h1>{cell.name}</h1>
    <p>Configuration:</p>
    <pre><code>{cell.configuration}</code></pre>
    <p>Port: {cell.port}</p>
</div>
