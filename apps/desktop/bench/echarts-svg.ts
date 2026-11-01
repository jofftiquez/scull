// ECharts with the SVG renderer.
import { SVGRenderer } from 'echarts/renderers';
import { echarts, makeRenderer } from './echarts-common';
import { runBench } from './harness';

echarts.use([SVGRenderer]);
void runBench(makeRenderer('svg'));
