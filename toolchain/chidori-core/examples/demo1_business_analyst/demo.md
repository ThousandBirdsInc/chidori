# This agent demonstrates how to traverse multiple parts of a website, extract data, and produce a structured report.

We use Astral rather than Playwright for Deno support
https://astral.deno.dev/guides/navigation/

```ts
import { launch } from "https://deno.land/x/astral/mod.ts";


// Connect to remote endpoint
const browser = await launch({
    wsEndpoint: `wss://connect.browserbase.com?apiKey=${Deno.env.get("BROWSERBASE_API_KEY")}`
});

// Do stuff
// Open the webpage
const page = await browser.newPage("https://deno.land");

// Click the search button
const button = await page.$("button");
await button!.click();

// Type in the search input
const input = await page.$("#search-input");
await input!.type("pyro", { delay: 1000 });

// Wait for the search results to come back
await page.waitForNetworkIdle({ idleConnections: 0, idleTime: 1000 });

// Click the 'pyro' link
const xLink = await page.$("a.justify-between:nth-child(1)");
await Promise.all([
    xLink!.click(),
    page.waitForNavigation(),
]);

// Click the link to 'pyro.deno.dev'
const dLink = await page.$(
    ".markdown-body > p:nth-child(8) > a:nth-child(1)",
);
await Promise.all([
    dLink!.click(),
    page.waitForNavigation(),
]);


// Close connection
await browser.close();
```



