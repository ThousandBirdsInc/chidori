<script>
    export let cell;
</script>

<div>
    <h3>Name: {cell.name}</h3>
    <p>Provider: {cell.provider}</p>
    <p>
        {JSON.stringify(cell)}
    </p>
</div>
