// Default theme without the bundled Inter font: the thousandbirds.ai design
// uses the system sans stack, so shipping Inter would just be dead weight.
import DefaultTheme from 'vitepress/theme-without-fonts'
import './custom.css'

export default DefaultTheme
