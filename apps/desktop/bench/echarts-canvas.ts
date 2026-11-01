// ECharts with the canvas renderer (its default).
import { CanvasRenderer } from 'echarts/renderers';
import { echarts, makeRenderer } from './echarts-common';
import { runBench } from './harness';

echarts.use([CanvasRenderer]);
void runBench(makeRenderer('canvas'));
