# This agent demonstrate scraping a website, when the website changes, the agent will automatically update itself to reflect the changes.


# This is a simple function that initially will scrape a website, extract some information, and take a screenshot
```js

const puppeteer = require('puppeteer');

async function scrapeWebsite() {
    const browser = await puppeteer.launch(); // Launch the browser
    const page = await browser.newPage(); // Open a new page

    await page.goto('https://example.com'); // Navigate to the website

    // Take a screenshot and save it to 'example.png'
    await page.screenshot({ path: 'example.png' });

    // Extract the text of the h1 element
    const headingText = await page.evaluate(() => document.querySelector('h1').innerText);

    console.log('Heading text:', headingText); // Log the heading text

    await browser.close(); // Close the browser
}

scrapeWebsite().catch(console.error);
```


