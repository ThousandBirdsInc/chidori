import json
import os
from PIL import Image, ImageDraw, ImageFont

# Grid settings
CELL_SIZE = 40
BORDER_WIDTH = 2
PADDING = 40

# Colors
COLORS = [
    (0, 0, 0),  # 0: Black
    (0, 116, 217),  # 1: Blue
    (255, 65, 54),  # 2: Red
    (46, 204, 64),  # 3: Green
    (255, 220, 0),  # 4: Yellow
    (170, 170, 170),  # 5: Grey
    (240, 18, 190),  # 6: Fuchsia
    (255, 133, 27),  # 7: Orange
    (127, 219, 255),  # 8: Teal
    (135, 12, 37)  # 9: Brown
]


def parse_json_file(file_path):
    with open(file_path, 'r') as file:
        data = json.load(file)
    return data['test'] + data['train']


def draw_grid(draw, grid, start_x, start_y, cell_size):
    for row in range(len(grid)):
        for col in range(len(grid[row])):
            x = start_x + col * (cell_size + BORDER_WIDTH)
            y = start_y + row * (cell_size + BORDER_WIDTH)
            color = COLORS[grid[row][col]]
            draw.rectangle([x, y, x + cell_size, y + cell_size], fill=color)


def create_example_image(input_grid, output_grid, index):
    input_rows, input_cols = len(input_grid), len(input_grid[0])
    output_rows, output_cols = len(output_grid), len(output_grid[0])

    max_cols = max(input_cols, output_cols)
    max_rows = max(input_rows, output_rows)

    image_width = 2 * PADDING + 2 * max_cols * (CELL_SIZE + BORDER_WIDTH) + PADDING
    image_height = 2 * PADDING + max_rows * (CELL_SIZE + BORDER_WIDTH) + 30  # Extra space for labels

    image = Image.new('RGB', (image_width, image_height), color='white')
    draw = ImageDraw.Draw(image)

    # Draw input grid
    draw_grid(draw, input_grid, PADDING, PADDING + 30, CELL_SIZE)

    # Draw output grid
    output_start_x = PADDING + max_cols * (CELL_SIZE + BORDER_WIDTH) + PADDING
    draw_grid(draw, output_grid, output_start_x, PADDING + 30, CELL_SIZE)

    # Add labels
    font = ImageFont.load_default()
    draw.text((PADDING, 10), "Input", fill='black', font=font)
    draw.text((output_start_x, 10), "Output", fill='black', font=font)

    # Save the image
    image.save(f'example_{index}.png')


def process_json_files(directory):
    json_files = [f for f in os.listdir(directory) if f.endswith('.json')]

    for file_index, json_file in enumerate(json_files):
        data = parse_json_file(os.path.join(directory, json_file))

        print(f"Processing file: {json_file}")
        for i, example in enumerate(data):
            input_grid = example['input']
            output_grid = example['output']
            create_example_image(input_grid, output_grid, f"{file_index}_{i}")

        print(f"Processed {len(data)} examples from {json_file}")


# Run the image creation with the provided directory
directory = "/Users/coltonpierson/reference/ARC-AGI/data/training"  # Replace with the actual path to your JSON files
process_json_files(directory)
