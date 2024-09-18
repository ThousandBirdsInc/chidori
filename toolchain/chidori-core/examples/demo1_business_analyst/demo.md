# This agent demonstrates how to traverse multiple parts of a website, extract data, and produce a structured report.

We use Astral rather than Playwright for Deno support
https://astral.deno.dev/guides/navigation/

```ts
// Import Astral
import { launch } from "https://deno.land/x/astral/mod.ts";

// Launch the browser
const browser = await launch();

// Open a new page
const page = await browser.newPage("https://deno.land");

// Take a screenshot of the page and save that to disk
// TODO: doing this in a closure so we don't attempt to serialize the output, we don't support binary arrays yet
(async () => {
    const screenshot = await page.screenshot();
    Deno.writeFileSync("screenshot.png", screenshot);
})()

// Close the browser
await browser.close();
```



